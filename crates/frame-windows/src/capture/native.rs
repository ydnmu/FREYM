//! DXGI Desktop Duplication and D3D11 native session (FRAME-0103, ADR 0005).
//!
//! This is the only module in the capture path that performs native calls, and it is
//! the reviewed FFI boundary of the crate's `unsafe_code = "deny"` policy. Every
//! `unsafe` block carries a SAFETY comment; there are no handwritten SDK interfaces,
//! vtables or custom COM reference counting, and no WGC/GDI/WARP/other-display
//! fallback capture exists anywhere on this path. GDI is used only to draw the
//! frame's pointer shape onto the already captured GPU texture when DXGI reports a
//! separate hardware pointer.
//!
//! The device and textures live in [`NativeBacking`], which outstanding consumer
//! leases keep alive; context, queries and staging remain worker-owned. Releasing an outstanding
//! texture after the session stopped is safe because D3D11 and DXGI objects are
//! free-threaded: `CoUninitialize` on the worker ends that thread's apartment but
//! does not destroy objects that still have references.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::Arc;
use std::time::{Duration, Instant};

use frame_capture::CaptureCancellation;
use frame_contracts::RecorderError;
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, HANDLE, HMODULE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RESOURCE_MISC_GDI_COMPATIBLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Query, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_UNSUPPORTED,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutput6,
    IDXGIOutputDuplication, IDXGIResource, IDXGISurface1,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{BOOL, Interface};

use super::diagnostics::{Counters, NativeDescription, PixelSample, READBACK_PIXEL_SAMPLES};
use super::worker::{
    Acquired, CaptureSource, FrameObservation, ReadbackOutcome, ReadbackSample, internal_error,
    merge_release_failure, not_implemented_error, permission_denied_error, source_lost_error,
};
use crate::capture::CapturePreparation;
use crate::color_convert::{ColorDirection, GpuColorPass};
use crate::cursor_preview_diagnostics::{self as cursor_diagnostics, CaptureTrace};
use crate::display::CaptureOutput;
use crate::preview::PreviewSource;
use crate::surface::{
    EncodeTexture, MAX_PAYLOAD_BYTES, OwnedPayloadPlan, PayloadLifetime, PayloadOwner, SLOT_COUNT,
    SharedPayload, SurfaceFormat, SurfaceIdentity, SurfaceSignal, plan_owned_payload,
    plan_owned_payload_with_bytes,
};

/// The API this proof uses, recorded in diagnostics.
const CAPTURE_API: &str = "DXGI Desktop Duplication (IDXGIOutput1::DuplicateOutput) + D3D11";
/// Bound on the selection scan, so a broken driver cannot make it unbounded.
const MAX_SCANNED_OUTPUTS: usize = 64;
/// Bound on readback readiness polling before it becomes an explicit failure.
const READBACK_MAP_WATCHDOG: Duration = Duration::from_secs(2);
/// The image format this proof copies.
const PIXEL_FORMAT: &str = "DXGI_FORMAT_B8G8R8A8_UNORM";

#[path = "cursor.rs"]
mod cursor;
use cursor::FramePointer;

struct CaptureDiagnosticState {
    source_id: u64,
    sequence: u64,
    current: Option<CaptureTrace>,
    copied: bool,
    software_compositions: u32,
}

impl CaptureDiagnosticState {
    fn new() -> Option<Self> {
        cursor_diagnostics::enabled().then(|| Self {
            source_id: cursor_diagnostics::new_source_id(),
            sequence: 0,
            current: None,
            copied: false,
            software_compositions: 0,
        })
    }

    fn acquired(&mut self) -> CaptureTrace {
        self.sequence = self.sequence.saturating_add(1);
        let trace = CaptureTrace {
            source_id: self.source_id,
            sequence: self.sequence,
            acquired_ns: cursor_diagnostics::now_ns(),
        };
        self.current = Some(trace);
        self.copied = false;
        self.software_compositions = 0;
        trace
    }
}

/// COM apartment guard: every successful initialization is balanced exactly once, on
/// the same thread.
pub(crate) struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    /// Initializes MTA on the calling (worker) thread. The caller's apartment is not
    /// touched, because the worker is a fresh thread that only this session uses.
    pub(crate) fn initialize_mta() -> Result<Self, RecorderError> {
        // SAFETY: CoInitializeEx has no preconditions beyond a live thread; a null
        // reserved pointer is documented as required.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Err(internal_error(
                "The capture worker thread was already initialized in a different COM apartment, so desktop duplication cannot own the thread."
                    .to_string(),
            ));
        }
        if result.is_err() {
            return Err(internal_error(format!(
                "CoInitializeEx(COINIT_MULTITHREADED) failed with HRESULT 0x{:08X}, so the capture worker cannot use DXGI.",
                result.0
            )));
        }
        // S_OK and S_FALSE both require a matching CoUninitialize.
        Ok(Self { initialized: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances this thread's successful CoInitializeEx exactly once.
            unsafe { CoUninitialize() };
            self.initialized = false;
        }
    }
}

/// Exactly what an outstanding consumer lease must keep alive: the device and the
/// four owned textures. It deliberately holds no immediate context, query or staging
/// texture, so a lease that outlives its session retains only the backing it needs and
/// can never touch the worker's context.
///
/// Held through an `Arc`; the [`PayloadLifetime`] counters observe its construction and
/// destruction.
pub(super) struct NativeBacking {
    /// Held for ownership: D3D11 resources belong to their device, so the device must
    /// outlive every texture in this payload, including leases that outlive the session.
    _device: ID3D11Device,
    textures: Vec<ID3D11Texture2D>,
    width: u32,
    height: u32,
    /// The capture pool this backing belongs to, compared by the encoder so a lease
    /// from another session can never be registered here.
    pool: u64,
    /// Registered with the session's payload-lifetime counters.
    lifetime: Arc<PayloadLifetime>,
    format: SurfaceFormat,
}

impl PayloadOwner for NativeBacking {
    fn format(&self) -> SurfaceFormat {
        self.format
    }
    fn signal(&self) -> Option<SurfaceSignal> {
        (self.format == SurfaceFormat::ScRgbFloat16).then_some(SurfaceSignal::SCRGB)
    }
    fn encode_texture(&self, slot_index: usize) -> Option<EncodeTexture> {
        let texture = self.textures.get(slot_index)?;
        let device = self._device.as_raw();
        Some(EncodeTexture {
            device,
            texture: texture.as_raw(),
            identity: SurfaceIdentity {
                pool: self.pool,
                device: device as usize,
                texture: texture.as_raw() as usize,
                width: self.width,
                height: self.height,
            },
        })
    }
}

impl NativeBacking {
    /// One of this session's own completed textures, for the preview readback
    /// (FRAME-0208).
    ///
    /// It hands over a typed handle to a slot this backing owns and keeps alive, plus the
    /// geometry the encoder is configured for. `PreviewSource` takes its own COM
    /// reference, so the handle stays valid even though the slot outlives the caller, and
    /// the worker's preview generator releases exactly that reference.
    pub(super) fn preview_source(&self, slot_index: usize) -> Option<PreviewSource> {
        let texture = self.textures.get(slot_index)?;
        Some(PreviewSource::new(texture, self.width, self.height))
    }

    /// The session's own device, for the preview generator.
    pub(super) fn device_handle(&self) -> Option<ID3D11Device> {
        Some(self._device.clone())
    }
}

impl Drop for NativeBacking {
    fn drop(&mut self) {
        self.lifetime.owner_dropped();
    }
}

/// Private idle wait outside the capture device's native calls.
struct CaptureIdleWait(HANDLE);

/// Scoped media scheduling on this worker only. It avoids ordinary background
/// scheduling delaying a ready capture timer past a complete display period.
/// Windows retains its responsiveness/quota policy. The scoped performance policy
/// may request HighQoS on this worker; registry, process priority, system timer
/// resolution and affinity stay unchanged.
pub struct CaptureScheduling {
    handle: HANDLE,
    policy: crate::performance::ThreadPolicy,
    _same_thread: PhantomData<*mut ()>,
}

impl CaptureScheduling {
    /// Associate this calling media worker with Windows' standard Capture task.
    /// The guard must remain on this thread and reverts membership on drop.
    pub fn join() -> Option<Self> {
        let mut index = 0;
        // SAFETY: static NUL-terminated standard task name and valid index output.
        match unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Capture"), &mut index) } {
            Ok(handle) => {
                cursor_diagnostics::record("capture_scheduling", None, || {
                    serde_json::json!({"task": "Capture", "registered": true, "task_index": index,
                        "thread": std::thread::current().name()})
                });
                let mut guard = Self {
                    handle,
                    policy: Default::default(),
                    _same_thread: PhantomData,
                };
                guard.refresh();
                Some(guard)
            }
            Err(error) => {
                crate::performance::unavailable(format!(
                    "Windows media scheduling unavailable: {error}"
                ));
                // MMCSS can be disabled by machine policy. Recording remains
                // available with ordinary scheduling; diagnostics retain the limit.
                cursor_diagnostics::record("capture_scheduling", None, || {
                    serde_json::json!({"task": "Capture", "registered": false, "error": error.to_string(),
                        "thread": std::thread::current().name()})
                });
                None
            }
        }
    }
    pub fn refresh(&mut self) {
        self.policy.refresh(self.handle);
    }
}

impl Drop for CaptureScheduling {
    fn drop(&mut self) {
        // SAFETY: this non-Send guard lives and drops on the registering worker.
        let result = unsafe { AvRevertMmThreadCharacteristics(self.handle) };
        cursor_diagnostics::record(
            "capture_scheduling_reverted",
            None,
            || serde_json::json!({"success": result.is_ok(), "error": result.err().map(|e| e.to_string())}),
        );
    }
}

impl CaptureIdleWait {
    fn new() -> Result<Self, RecorderError> {
        // SAFETY: unnamed, private auto-reset timer; this worker owns the handle.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|e| map_native_error("CreateWaitableTimerExW(capture idle)", e))?;
        Ok(Self(handle))
    }

    fn wait(&self) -> Result<(), RecorderError> {
        // Relative 1 ms in 100 ns units. No device/context lock is held here;
        // no global timer-resolution or power setting is changed.
        let due = -10_000i64;
        // SAFETY: private live timer, valid due-time pointer, no APC callback.
        unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) }
            .map_err(|e| map_native_error("SetWaitableTimer(capture idle)", e))?;
        // SAFETY: the worker owns the handle; timeout also bounds cancellation latency.
        match unsafe { WaitForSingleObject(self.0, 5) } {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => Ok(()),
            _ => Err(internal_error("The capture idle timer wait failed.".into())),
        }
    }
}

impl Drop for CaptureIdleWait {
    fn drop(&mut self) {
        // SAFETY: closes this worker's timer exactly once, after any wait returned.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// Keep idle waiting structurally outside the DXGI call and its native-call scope.
// `timeout_ms` is the worker's idle/pending hint, never a DXGI blocking timeout.
fn poll_desktop(
    timeout_ms: u32,
    poll: impl FnOnce(u32) -> windows::core::Result<()>,
    idle: impl FnOnce() -> Result<(), RecorderError>,
) -> Result<windows::core::Result<()>, RecorderError> {
    let result = poll(0);
    if timeout_ms > 0 && matches!(&result, Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT) {
        idle()?;
    }
    Ok(result)
}

#[cfg(test)]
mod acquisition_wait_tests {
    use super::*;

    #[test]
    fn idle_wait_releases_the_device_to_the_encoder() {
        let device = std::sync::Mutex::new(());
        let result = poll_desktop(
            5,
            |timeout| {
                assert_eq!(timeout, 0, "DXGI must never hold the device while waiting");
                let _device_call = device.lock().unwrap();
                Err(windows::core::Error::from_hresult(DXGI_ERROR_WAIT_TIMEOUT))
            },
            || {
                assert!(
                    device.try_lock().is_ok(),
                    "encoder can enter during source idle wait"
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result.unwrap_err().code(), DXGI_ERROR_WAIT_TIMEOUT);
    }

    #[test]
    fn pending_gpu_work_and_ready_frames_never_idle_wait() {
        for ready in [false, true] {
            let result = poll_desktop(
                0,
                |timeout| {
                    assert_eq!(timeout, 0);
                    if ready {
                        Ok(())
                    } else {
                        Err(windows::core::Error::from_hresult(DXGI_ERROR_WAIT_TIMEOUT))
                    }
                },
                || panic!("pending copies must be polled promptly"),
            )
            .unwrap();
            assert_eq!(result.is_ok(), ready);
        }
        assert!(
            poll_desktop(5, |_| Ok(()), || panic!("ready image must not wait"))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn native_failure_is_preserved_and_timer_failure_is_explicit() {
        let failure = poll_desktop(
            5,
            |_| Err(windows::core::Error::from_hresult(DXGI_ERROR_ACCESS_LOST)),
            || panic!("a lost source must not wait"),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(failure.code(), DXGI_ERROR_ACCESS_LOST);
        let failure = poll_desktop(
            5,
            |_| Err(windows::core::Error::from_hresult(DXGI_ERROR_WAIT_TIMEOUT)),
            || Err(internal_error("injected timer failure".into())),
        )
        .unwrap_err();
        assert_eq!(failure.message, "injected timer failure");
    }
}

/// The DXGI capture source. Created on the worker thread and used only there.
pub(super) struct DxgiSource {
    witness: crate::witness::Witness,
    diagnostic_state: Option<CaptureDiagnosticState>,
    /// Worker-only resources: the immediate context, this session's copies and the
    /// opt-in staging texture. They die with the worker and are never reachable from a
    /// lease.
    context: ID3D11DeviceContext,
    queries: Vec<ID3D11Query>,
    staging: Option<ID3D11Texture2D>,
    backing: Arc<NativeBacking>,
    description: NativeDescription,
    duplication: Option<IDXGIOutputDuplication>,
    pending_output: Option<(IDXGIOutput, CaptureOutput, bool)>,
    idle_wait: CaptureIdleWait,
    ticks_per_second: u64,
    /// Tracks the documented exactly-once `ReleaseFrame` obligation.
    acquired_outstanding: bool,
    /// A timeout may publish a cursor update without acquiring a DXGI frame.
    synthetic_outstanding: bool,
    /// Uncomposited DXGI image retained for separate-pointer movement. The
    /// image itself can contain the pointer when DXGI reports it as embedded.
    clean_desktop: Option<ID3D11Texture2D>,
    clean_desktop_ready: bool,
    /// Only a retained image captured with a separate pointer is cursor-free.
    clean_desktop_separate_pointer: bool,
    /// The retained background must include every desktop presentation seen,
    /// even if admission dropped the acquisition that carried that update.
    clean_desktop_present_time: i64,
    latest_desktop_present_time: i64,
    show_cursor: bool,
    separate_pointer_visible: bool,
    /// Pointer updates also refresh an embedded pointer in the retained image.
    pending_pointer_visibility: Option<bool>,
    pending_desktop_present_time: i64,
    pointer: FramePointer,
    replay_sampling: bool,
    replay_sample_after: Instant,
    payload_bytes: u64,
    /// Reserved for one same-size FP16 shader input, allocated only on format 10.
    sdr_input_bytes: u64,
    sdr_converter: Option<(ID3D11Texture2D, GpuColorPass)>,
    last_copied_slot: Option<usize>,
    /// Checked between bounded readback polls.
    cancellation: CaptureCancellation,
    /// Shared counters, so bounded readback polling is observable.
    counters: Arc<Counters>,
    _scheduling: Option<CaptureScheduling>,
    /// Dropped last, so this thread's COM apartment is uninitialized after the
    /// session's own references.
    _apartment: ComApartment,
}

impl CaptureSource for DxgiSource {
    type Image = IDXGIResource;

    fn begin_frames(&mut self) -> Result<(), RecorderError> {
        if let Some((output, selection, hdr_float)) = self.pending_output.take() {
            self.duplication = Some(open_duplication(
                &output,
                &self.backing._device,
                &selection,
                hdr_float,
            )?);
            // SAFETY: newly activated duplication owned by this worker.
            self.description.desktop_image_in_system_memory = unsafe {
                self.duplication
                    .as_ref()
                    .expect("activated duplication")
                    .GetDesc()
                    .DesktopImageInSystemMemory
                    .as_bool()
            };
            cursor_diagnostics::record(
                "dxgi.activated",
                None,
                || serde_json::json!({"width": self.backing.width, "height": self.backing.height}),
            );
        }
        Ok(())
    }

    fn set_replay_sampling(&mut self, enabled: bool) -> Result<(), RecorderError> {
        if enabled && self.clean_desktop.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: the capture-owned texture and initialized descriptor are valid.
            unsafe { self.texture(0)?.GetDesc(&mut desc) };
            let bytes = u64::from(desc.Width)
                * u64::from(desc.Height)
                * if self.backing.format == SurfaceFormat::ScRgbFloat16 {
                    8
                } else {
                    4
                };
            if self
                .payload_bytes
                .saturating_add(bytes)
                .saturating_add(self.sdr_input_bytes)
                > MAX_PAYLOAD_BYTES
            {
                return Err(internal_error(
                    "Replay still-image texture exceeds the capture payload budget.".into(),
                ));
            }
            desc.BindFlags = 0;
            desc.MiscFlags = 0;
            let mut texture = None;
            // SAFETY: initialized descriptor, same capture device, owned output.
            unsafe {
                self.backing
                    ._device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            }
            .map_err(|e| map_native_error("CreateTexture2D(replay still)", e))?;
            let texture =
                texture.ok_or_else(|| internal_error("Replay still texture is missing.".into()))?;
            if let Some(slot) = self.last_copied_slot {
                // SAFETY: both textures belong to this worker's context. Copy
                // commands execute in FIFO order, including a pending slot copy.
                unsafe { self.context.CopyResource(&texture, self.texture(slot)?) };
                self.clean_desktop_ready = true;
            }
            self.clean_desktop = Some(texture);
            self.payload_bytes = self.payload_bytes.saturating_add(bytes);
        }
        self.replay_sampling = enabled;
        Ok(())
    }

    fn acquire(&mut self, timeout_ms: u32) -> Result<Option<Acquired<Self::Image>>, RecorderError> {
        if let Some(scheduling) = &mut self._scheduling {
            scheduling.refresh();
        }
        if self.acquired_outstanding {
            return Err(internal_error(
                "AcquireNextFrame was requested before the previously acquired desktop frame was released."
                    .to_string(),
            ));
        }
        // SAFETY: an all-zero DXGI_OUTDUPL_FRAME_INFO is a valid out-parameter state,
        // and both pointers stay valid for the duration of the call.
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let duplication = self.duplication.as_ref().ok_or_else(|| {
            internal_error("DXGI acquisition attempted before source activation.".into())
        })?;
        // SAFETY: the duplication object is alive and owned by this worker thread,
        // which is the thread that created it. NEVER wait inside DXGI on the
        // capture device: NVENC's map/lock/unlock calls also use this device and
        // can otherwise starve behind repeated blocking acquisitions.
        let result = poll_desktop(
            timeout_ms,
            |native_timeout| {
                let _phase = self.witness.enter(c"capture/AcquireNextFrame");
                // SAFETY: worker-owned duplication and live initialized out-parameters.
                unsafe { duplication.AcquireNextFrame(native_timeout, &mut info, &mut resource) }
            },
            || {
                let started = self
                    .diagnostic_state
                    .as_ref()
                    .map(|_| cursor_diagnostics::now_ns());
                let result = self.idle_wait.wait();
                cursor_diagnostics::record("dxgi.idle_wait", None, || {
                    serde_json::json!({
                        "source_id": self.diagnostic_state.as_ref().map(|state| state.source_id),
                        "requested_ns": 1_000_000,
                        "started_ns": started,
                        "elapsed_ns": started.map(|start| cursor_diagnostics::now_ns().saturating_sub(start)),
                        "success": result.is_ok(),
                    })
                });
                result
            },
        )?;
        match result {
            Ok(()) => {
                self.acquired_outstanding = true;
                let trace = self
                    .diagnostic_state
                    .as_mut()
                    .map(CaptureDiagnosticState::acquired);
                cursor_diagnostics::record("dxgi.acquire", trace, || {
                    let mut sampled_qpc = 0;
                    // SAFETY: diagnostic-only QPC sample; failure is reported,
                    // never substituted for the source's native timestamp.
                    let sampled = unsafe { QueryPerformanceCounter(&mut sampled_qpc) }.is_ok();
                    serde_json::json!({
                        "last_present_qpc": info.LastPresentTime,
                        "last_mouse_update_qpc": info.LastMouseUpdateTime,
                        "sampled_qpc": sampled.then_some(sampled_qpc),
                        "qpc_frequency": self.ticks_per_second,
                        "accumulated_frames": info.AccumulatedFrames,
                        "pointer_metadata_valid": info.LastMouseUpdateTime != 0,
                        "raw_pointer_visible": info.PointerPosition.Visible.as_bool(),
                        "raw_pointer_x": info.PointerPosition.Position.x,
                        "raw_pointer_y": info.PointerPosition.Position.y,
                        "pointer_shape_buffer_bytes": info.PointerShapeBufferSize,
                        "protected_content_masked_out": info.ProtectedContentMaskedOut.as_bool(),
                        "show_cursor": self.show_cursor,
                        "surface_width": self.backing.width,
                        "surface_height": self.backing.height,
                        "embedded_in_pixels": "unknown",
                        "synthetic_replay": false,
                    })
                });
                self.pending_desktop_present_time = info.LastPresentTime;
                self.latest_desktop_present_time =
                    self.latest_desktop_present_time.max(info.LastPresentTime);
                self.pending_pointer_visibility =
                    (info.LastMouseUpdateTime > 0).then(|| info.PointerPosition.Visible.as_bool());
                // DXGI reports changes, not the complete pointer state on every
                // frame. Preserve the observation even if pressure drops this
                // image, or a later software-pointer image gets drawn twice.
                if let Some(visible) = self.pending_pointer_visibility {
                    self.separate_pointer_visible = visible;
                }
                if self.show_cursor
                    && self.backing.format == SurfaceFormat::Bgra8
                    && let Err(error) = self.pointer.update(duplication, &info)
                {
                    cursor_diagnostics::record(
                        "dxgi.cursor_error",
                        trace,
                        || serde_json::json!({"error": error.message, "software_composition_count": 0}),
                    );
                    return match self.release_frame() {
                        Ok(()) => Err(error),
                        Err(release) => Err(merge_release_failure(Some(error), release)),
                    };
                }
                cursor_diagnostics::record("dxgi.cursor_state", trace, || {
                    self.pointer.diagnostic_fields()
                });
                let observation = FrameObservation {
                    last_present_time: info.LastPresentTime,
                    last_mouse_update_time: if self.show_cursor {
                        info.LastMouseUpdateTime
                    } else {
                        0
                    },
                    synthetic_cursor_time: false,
                    synthetic_replay_time: false,
                    accumulated_frames: info.AccumulatedFrames,
                    protected_content_masked_out: info.ProtectedContentMaskedOut.as_bool(),
                    trace,
                };
                match resource {
                    Some(image) => Ok(Some(Acquired { image, observation })),
                    None => {
                        // A successful acquire without a resource would still hold the
                        // frame, so it is released here instead of leaking it, and the
                        // missing-resource failure stays primary if release also fails.
                        let missing = internal_error(
                            "AcquireNextFrame reported success without returning a desktop resource."
                                .to_string(),
                        );
                        match self.release_frame() {
                            Ok(()) => Err(missing),
                            Err(release) => Err(merge_release_failure(Some(missing), release)),
                        }
                    }
                }
            }
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                if self.replay_sampling
                    && self.clean_desktop_ready
                    && self.clean_desktop_present_time == self.latest_desktop_present_time
                    && Instant::now() >= self.replay_sample_after
                {
                    let clean = self.clean_desktop.as_ref().expect("retained desktop");
                    let mut now = 0;
                    // SAFETY: initialized QPC out-parameter, same clock mapping.
                    unsafe { QueryPerformanceCounter(&mut now) }.map_err(|e| {
                        map_native_error("QueryPerformanceCounter(replay still)", e)
                    })?;
                    let image = clean
                        .cast()
                        .map_err(|e| map_native_error("Replay desktop cast", e))?;
                    self.synthetic_outstanding = true;
                    self.replay_sample_after = Instant::now() + Duration::from_millis(250);
                    let trace = self
                        .diagnostic_state
                        .as_mut()
                        .map(CaptureDiagnosticState::acquired);
                    cursor_diagnostics::record("dxgi.replay_reuse", trace, || {
                        serde_json::json!({
                            "synthetic_replay_qpc": now,
                            "qpc_frequency": self.ticks_per_second,
                            "clean_cache_separate_pointer": self.clean_desktop_separate_pointer,
                            "pointer": self.pointer.diagnostic_fields(),
                            "embedded_in_pixels": "unknown",
                        })
                    });
                    return Ok(Some(Acquired {
                        image,
                        observation: FrameObservation {
                            last_mouse_update_time: now,
                            synthetic_replay_time: true,
                            trace,
                            ..FrameObservation::default()
                        },
                    }));
                }
                // DXGI wakes AcquireNextFrame for pointer-only changes too. A
                // timeout supplies no newer pointer metadata: polling the OS
                // here would pair a future pointer with an older desktop frame.
                Ok(None)
            }
            Err(error) => Err(map_native_error("AcquireNextFrame", error)),
        }
    }

    fn begin_copy(&mut self, image: &Self::Image, slot_index: usize) -> Result<(), RecorderError> {
        let acquired_texture: ID3D11Texture2D = image.cast().map_err(|error| {
            internal_error(format!(
                "The acquired desktop resource is not a D3D11 texture: HRESULT 0x{:08X}.",
                error.code().0
            ))
        })?;
        // On this driver, a pointer-only acquisition can expose the output's
        // native FP16 resource even though desktop updates are BGRA8. An invalid
        // CopyResource is void and leaves old slot pixels, so drawing again would
        // accumulate cursors in both preview and encoder input. Reuse only a
        // current, cursor-free background from this same duplication session.
        let retained = self.clean_desktop.as_ref().filter(|_| {
            !self.synthetic_outstanding
                && self.pending_desktop_present_time == 0
                && self.clean_desktop_ready
                && self.clean_desktop_separate_pointer
                && self.separate_pointer_visible
                && self.clean_desktop_present_time == self.latest_desktop_present_time
        });
        let background = frame_background(&acquired_texture, self.texture(slot_index)?, retained)
            .map(|(texture, reused)| (texture.clone(), reused));
        let mut format_converted = false;
        let (texture, background_cache_reused) = match background {
            Ok(background) => background,
            Err(copy_error) => {
                // A real FP16 desktop update or first pointer-only frame has no
                // eligible cached BGRA background. Normalize its real pixels on
                // this same device instead of failing or drawing on an old slot.
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: live acquired resource, metadata only.
                unsafe { acquired_texture.GetDesc(&mut desc) };
                if self.backing.format != SurfaceFormat::Bgra8
                    || desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT
                    || desc.Width != self.backing.width
                    || desc.Height != self.backing.height
                    || desc.ArraySize != 1
                    || desc.MipLevels != 1
                    || desc.SampleDesc.Count != 1
                    || desc.SampleDesc.Quality != 0
                {
                    return Err(copy_error);
                }
                self.normalize_sdr(&acquired_texture, slot_index)?;
                format_converted = true;
                (self.texture(slot_index)?.clone(), false)
            }
        };
        let query = self.query(slot_index)?.clone();
        self.last_copied_slot = Some(slot_index);
        let trace = self
            .diagnostic_state
            .as_ref()
            .and_then(|state| state.current);
        let started_ns = trace.map(|_| cursor_diagnostics::now_ns());
        let mut cache_refreshed = false;
        self.replay_sample_after = Instant::now() + Duration::from_millis(250);
        if !self.synthetic_outstanding && !background_cache_reused {
            // A rejected or stale acquisition must not replace the retained
            // image. A timeout uses only an image that reached an output slot.
            if let Some(clean) = &self.clean_desktop
                && (self.pending_desktop_present_time > 0
                    || !self.clean_desktop_ready
                    || self.clean_desktop_present_time != self.latest_desktop_present_time
                    || self.clean_desktop_separate_pointer != self.separate_pointer_visible
                    || self.pending_pointer_visibility == Some(false))
            {
                // SAFETY: both textures belong to this worker's D3D11 device.
                unsafe { self.context.CopyResource(clean, &texture) };
                self.clean_desktop_ready = true;
                self.clean_desktop_separate_pointer = self.separate_pointer_visible;
                self.clean_desktop_present_time = self.latest_desktop_present_time;
                cache_refreshed = true;
            }
        }
        // A cursor-only timeout uses the uncomposited desktop copy, never a
        // slot with an old pointer already drawn into it.
        let target = self.texture(slot_index)?;
        let copy_started_ns = trace.map(|_| cursor_diagnostics::now_ns());
        if !format_converted {
            // SAFETY: selector validated layout and format, and source is not
            // the target. Converted images are already in this output slot.
            unsafe {
                let phase = self.witness.enter(c"capture/CopyResource");
                self.context.CopyResource(target, &texture);
                drop(phase);
            }
        }
        let copy_finished_ns = trace.map(|_| cursor_diagnostics::now_ns());
        if self.show_cursor
            && self.backing.format == SurfaceFormat::Bgra8
            && self.separate_pointer_visible
            && (!self.synthetic_outstanding || self.clean_desktop_separate_pointer)
        {
            let draw_started_ns = trace.map(|_| cursor_diagnostics::now_ns());
            let drawn = draw_cursor_on_surface(target, &self.pointer, trace);
            let draw_finished_ns = trace.map(|_| cursor_diagnostics::now_ns());
            let successful_composition = drawn.is_ok() && self.pointer.visible();
            if successful_composition && let Some(state) = self.diagnostic_state.as_mut() {
                state.software_compositions = state.software_compositions.saturating_add(1);
            }
            cursor_diagnostics::record("dxgi.cursor_composition", trace, || {
                serde_json::json!({
                    "slot": slot_index,
                    "success": drawn.is_ok(),
                    "software_composition_count": drawn.is_ok().then_some(u32::from(successful_composition)),
                    "draw_surface_cpu_ns": draw_started_ns.zip(draw_finished_ns).map(|(start,end)| end.saturating_sub(start)),
                    "embedded_in_pixels": "unknown",
                    "pointer": self.pointer.diagnostic_fields(),
                })
            });
            drawn?;
        }
        // SAFETY: GDI's DC was released above before issuing another D3D command.
        // The query is per-slot, so a later poll cannot observe another slot.
        let flush_started_ns = trace.map(|_| cursor_diagnostics::now_ns());
        unsafe {
            let phase = self.witness.enter(c"capture/EndQuery");
            self.context.End(&query);
            drop(phase);
            let _phase = self.witness.enter(c"capture/Flush");
            self.context.Flush();
        }
        let flush_finished_ns = trace.map(|_| cursor_diagnostics::now_ns());
        if let Some(state) = self.diagnostic_state.as_mut() {
            state.copied = true;
        }
        cursor_diagnostics::record("dxgi.copy_submitted", trace, || {
            serde_json::json!({
                "slot": slot_index,
                "synthetic_replay": self.synthetic_outstanding,
                "cache_refreshed": cache_refreshed,
                "background_cache_reused": background_cache_reused,
                "fp16_to_sdr_gpu_conversion": format_converted,
                "cached_desktop_present_qpc": self.clean_desktop_present_time,
                "latest_desktop_present_qpc": self.latest_desktop_present_time,
                "clean_cache_ready": self.clean_desktop_ready,
                "clean_cache_separate_pointer": self.clean_desktop_separate_pointer,
                "separate_pointer_visible": self.separate_pointer_visible,
                "software_composition_count": self.diagnostic_state.as_ref().map_or(0, |state| state.software_compositions),
                "copy_and_cursor_commands_cpu_ns": started_ns.zip(flush_started_ns).map(|(start,end)| end.saturating_sub(start)),
                "slot_copy_commands_cpu_ns": copy_started_ns.zip(copy_finished_ns).map(|(start,end)| end.saturating_sub(start)),
                "flush_commands_cpu_ns": flush_started_ns.zip(flush_finished_ns).map(|(start,end)| end.saturating_sub(start)),
                "embedded_in_pixels": "unknown",
            })
        });
        Ok(())
    }

    fn query_complete(&mut self, slot_index: usize) -> Result<bool, RecorderError> {
        let query = self.query(slot_index)?;
        // The BOOL payload is initialized false on every poll: on this binding version
        // GetData maps S_FALSE into Ok(()), so Ok alone must never mean completion.
        let mut complete: BOOL = BOOL(0);
        // SAFETY: `complete` is a correctly sized, initialized BOOL payload for
        // D3D11_QUERY_EVENT and the query belongs to this worker's context.
        let result = unsafe {
            let _phase = self.witness.enter(c"capture/GetData");
            self.context.GetData(
                query,
                Some(&mut complete as *mut BOOL as *mut c_void),
                size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
        };
        match result {
            Ok(()) => Ok(complete.as_bool()),
            Err(error) => Err(map_native_error(
                "ID3D11DeviceContext::GetData(D3D11_QUERY_EVENT)",
                error,
            )),
        }
    }

    fn release_frame(&mut self) -> Result<(), RecorderError> {
        if self.synthetic_outstanding {
            self.synthetic_outstanding = false;
            if let Some(state) = self.diagnostic_state.as_mut() {
                cursor_diagnostics::record("dxgi.release", state.current, || {
                    serde_json::json!({
                        "synthetic_replay": true, "copied": state.copied,
                        "software_composition_count": state.software_compositions, "success": true,
                    })
                });
                state.current = None;
            }
            return Ok(());
        }
        if !self.acquired_outstanding {
            return Err(internal_error(
                "ReleaseFrame was requested without an outstanding acquired frame.".to_string(),
            ));
        }
        // SAFETY: this releases the frame acquired by the matching successful
        // AcquireNextFrame call on this thread, exactly once.
        let phase = self.witness.enter(c"capture/ReleaseFrame");
        let result = unsafe {
            self.duplication
                .as_ref()
                .expect("acquired duplication")
                .ReleaseFrame()
        };
        drop(phase);
        self.acquired_outstanding = false;
        self.pending_pointer_visibility = None;
        if let Some(state) = self.diagnostic_state.as_mut() {
            cursor_diagnostics::record("dxgi.release", state.current, || {
                serde_json::json!({
                    "synthetic_replay": false, "copied": state.copied,
                    "software_composition_count": state.software_compositions, "success": result.is_ok(),
                })
            });
            state.current = None;
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(map_native_error("ReleaseFrame", error)),
        }
    }

    fn readback(&mut self, slot_index: usize) -> ReadbackOutcome {
        let source = match self.texture(slot_index) {
            Ok(source) => source,
            Err(error) => return ReadbackOutcome::Failed(error),
        };
        let Some(staging) = self.staging.as_ref() else {
            return ReadbackOutcome::Failed(internal_error(
                "Opt-in pixel verification was requested without a staging texture.".to_string(),
            ));
        };
        let width = self.backing.width;
        let height = self.backing.height;

        // SAFETY: the staging texture is a single-mip, single-slice 2D texture of the
        // same size and format as the source and belongs to this device, and the copy
        // is queued on this worker's immediate context.
        unsafe {
            self.context.CopyResource(staging, source);
            self.context.Flush();
        }

        let mut poll = DeviceMapPoll {
            context: &self.context,
            staging,
            counters: Arc::clone(&self.counters),
            mapped: D3D11_MAPPED_SUBRESOURCE::default(),
        };
        // Bounded, cancellation-aware readiness polling instead of a blocking Map: a
        // busy resource reports DXGI_ERROR_WAS_STILL_DRAWING and the worker checks
        // cancellation and the watchdog between polls. A driver call itself still cannot
        // be forcibly cancelled, which is why cancellation is reported as an outcome
        // instead of being forced.
        match poll_until_mapped(
            &mut poll,
            &self.cancellation,
            &self.counters,
            READBACK_MAP_WATCHDOG,
        ) {
            Ok(PollOutcome::Cancelled) => ReadbackOutcome::Cancelled,
            Ok(PollOutcome::Mapped) => {
                // From here the mapping exists: `with_mapping` releases it exactly once
                // on every exit, including an unwind, and never when no map succeeded.
                let mapped = poll.mapped;
                with_mapping(&mut poll, || {
                    // SAFETY: `mapped` came from the successful Map above, and
                    // the valid BGRA rows lie in the live staging allocation and stay
                    // readable until this guard unmaps it. Padding need not be initialized.
                    match unsafe { MappedView::new(&mapped, width, height) } {
                        Ok(view) => ReadbackOutcome::Verified(view.readback_sample()),
                        Err(error) => ReadbackOutcome::Failed(error),
                    }
                })
            }
            Err(error) => ReadbackOutcome::Failed(error),
        }
    }

    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    fn sample_counter(&mut self) -> Result<i64, RecorderError> {
        let mut value = 0i64;
        // SAFETY: the out-parameter is a valid initialized integer.
        unsafe { QueryPerformanceCounter(&mut value) }
            .map_err(|error| map_native_error("QueryPerformanceCounter", error))?;
        Ok(value)
    }

    fn description(&self) -> &NativeDescription {
        &self.description
    }

    fn payload_owner(&self) -> SharedPayload {
        Arc::clone(&self.backing) as SharedPayload
    }

    fn preview_payload_bytes(&self) -> Option<u64> {
        // Replay can request its background later; account for that future
        // texture now so preview admission cannot spend its reserved memory.
        let replay_reserve = if self.clean_desktop.is_none() {
            u64::from(self.backing.width)
                * u64::from(self.backing.height)
                * if self.backing.format == SurfaceFormat::ScRgbFloat16 {
                    8
                } else {
                    4
                }
        } else {
            0
        };
        Some(
            self.payload_bytes
                .saturating_add(self.sdr_input_bytes)
                .saturating_add(replay_reserve),
        )
    }

    fn preview_source(&self, slot_index: usize) -> Option<PreviewSource> {
        self.backing.preview_source(slot_index)
    }

    fn preview_device(&self) -> Option<ID3D11Device> {
        self.backing.device_handle()
    }

    fn preview_context(&self) -> Option<ID3D11DeviceContext> {
        Some(self.context.clone())
    }
}

/// One non-blocking `Map` attempt.
pub(super) enum MapAttempt {
    Mapped,
    /// The documented busy result of `DO_NOT_WAIT`: retry within the bound.
    Busy,
    Failed(RecorderError),
}

/// Private seam over the map call, so the poll loop, its watchdog, its cancellation
/// checks and the exactly-once release are testable without a device.
pub(super) trait MapPoll {
    /// One `Map` attempt with `D3D11_MAP_FLAG_DO_NOT_WAIT`.
    fn attempt(&mut self) -> MapAttempt;
    /// Releases a mapping obtained by a successful [`MapPoll::attempt`].
    fn unmap(&mut self);
}

/// The outcome of one bounded poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PollOutcome {
    /// A mapping was obtained; the caller owns it until [`with_mapping`] returns.
    Mapped,
    /// The session was cancelled between polls. Not a failure.
    Cancelled,
}

/// Polls for a readable mapping, bounded by [`READBACK_MAP_WATCHDOG`] and by
/// cancellation.
///
/// No mapping is taken when cancellation or the watchdog wins, and a driver call that
/// itself cannot be interrupted is never forced: it is counted and retried between
/// checks.
pub(super) fn poll_until_mapped(
    poll: &mut impl MapPoll,
    cancellation: &CaptureCancellation,
    counters: &Counters,
    watchdog: Duration,
) -> Result<PollOutcome, RecorderError> {
    let deadline = Instant::now() + watchdog;
    loop {
        if cancellation.is_cancelled() {
            return Ok(PollOutcome::Cancelled);
        }
        match poll.attempt() {
            MapAttempt::Mapped => return Ok(PollOutcome::Mapped),
            MapAttempt::Busy => {
                crate::capture::SharedState::bump(&counters.readback_map_polls);
                if Instant::now() >= deadline {
                    return Err(internal_error(format!(
                        "The verification staging texture was still busy after {} ms, past the readback watchdog.",
                        watchdog.as_millis()
                    )));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            MapAttempt::Failed(error) => return Err(error),
        }
    }
}

/// Owns one successful mapping and releases it exactly once.
///
/// Created only after a successful map, so exactly one `Unmap` follows exactly one
/// `Map` on every path — a normal return, a validation error, or an unwind
/// (FRAME-0103-R2 finding 5).
pub(super) struct MappingGuard<'a, P: MapPoll> {
    poll: &'a mut P,
    active: bool,
}

impl<'a, P: MapPoll> MappingGuard<'a, P> {
    fn new(poll: &'a mut P) -> Self {
        Self { poll, active: true }
    }
}

impl<P: MapPoll> Drop for MappingGuard<'_, P> {
    fn drop(&mut self) {
        if self.active {
            // Release first, so a panicking unmap cannot leave the flag armed.
            self.active = false;
            self.poll.unmap();
        }
    }
}

/// Runs `body` while the mapping is held, releasing it exactly once afterwards.
pub(super) fn with_mapping<P: MapPoll, T>(poll: &mut P, body: impl FnOnce() -> T) -> T {
    let _guard = MappingGuard::new(poll);
    body()
}

/// A validated read-only view of one mapped staging texture.
///
/// Construction is the unsafe boundary: the caller must guarantee that the mapped
/// pointer addresses the bytes D3D11 promised for the mapping. Everything after
/// construction uses checked row slices, and the digest and the
/// sampled pixels only ever touch valid pixel rows — row padding is stride, never
/// pixel data.
pub(super) struct MappedView<'a> {
    /// Retain a pointer, not a slice spanning potentially uninitialized row padding.
    data: *const u8,
    _mapping: PhantomData<&'a [u8]>,
    row_pitch: usize,
    valid_row_bytes: usize,
    width: u32,
    height: u32,
    /// Bytes D3D11 reports for the whole mapping, for evidence only.
    mapped_bytes: usize,
}

impl<'a> MappedView<'a> {
    /// Validates the mapped layout and borrows the valid pixel rows.
    ///
    /// # Safety
    ///
    /// If layout validation succeeds, `mapped.pData` must address one allocation
    /// containing every row at `row * RowPitch`, with `width * 4` initialized,
    /// readable bytes per row. These pixel bytes must remain valid and unmodified
    /// for `'a`; row padding need not be initialized. The allocation must cover the
    /// checked span without address wraparound. Invalid layouts are rejected before
    /// dereferencing the pointer. Layout checks cannot establish pointer validity.
    pub(super) unsafe fn new(
        mapped: &D3D11_MAPPED_SUBRESOURCE,
        width: u32,
        height: u32,
    ) -> Result<Self, RecorderError> {
        if width == 0 || height == 0 {
            return Err(internal_error(format!(
                "The verification readback was asked for a {width}x{height} surface, which has no pixels to read."
            )));
        }
        if mapped.pData.is_null() {
            return Err(internal_error(
                "The verification staging texture mapped to a null pointer.".to_string(),
            ));
        }
        let row_pitch = mapped.RowPitch as usize;
        let valid_row_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| internal_error("The readback row width overflowed.".to_string()))?;
        if row_pitch < valid_row_bytes {
            return Err(internal_error(format!(
                "The mapped row pitch {} is smaller than the {valid_row_bytes} valid BGRA bytes of a {width} pixel row, so the pixels cannot be addressed safely.",
                mapped.RowPitch
            )));
        }
        let height = height as usize;
        let mapped_bytes = row_pitch
            .checked_mul(height)
            .ok_or_else(|| internal_error("The mapped staging size overflowed.".to_string()))?;
        // Bound every row offset and length without borrowing any padding.
        let span = row_pitch
            .checked_mul(height - 1)
            .and_then(|rows| rows.checked_add(valid_row_bytes))
            .ok_or_else(|| internal_error("The mapped pixel span overflowed.".to_string()))?;
        if span > isize::MAX as usize {
            return Err(internal_error(
                "The mapped pixel span exceeds Rust's pointer offset limit.".to_string(),
            ));
        }
        Ok(Self {
            data: mapped.pData.cast(),
            _mapping: PhantomData,
            row_pitch,
            valid_row_bytes,
            width,
            height: height as u32,
            mapped_bytes,
        })
    }

    /// The valid pixel bytes of one row, with row padding excluded.
    fn row(&self, index: usize) -> &[u8] {
        assert!(index < self.height as usize);
        let start = index * self.row_pitch;
        // SAFETY: new validates all row offsets/lengths and the isize bound. Its
        // caller guarantees a live allocation and initialized pixel bytes for each
        // row. This slice excludes padding and cannot outlive the borrowed view.
        unsafe { std::slice::from_raw_parts(self.data.add(start), self.valid_row_bytes) }
    }

    /// Every valid channel byte of every row, in order.
    pub(super) fn digest(&self) -> u64 {
        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        for index in 0..self.height as usize {
            for byte in self.row(index) {
                digest = (digest ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
        digest
    }

    /// One BGRA quadruple per equal-width column band at mid-height.
    pub(super) fn samples(&self) -> [PixelSample; READBACK_PIXEL_SAMPLES] {
        let mut samples = [PixelSample::default(); READBACK_PIXEL_SAMPLES];
        let sample_y = self.height / 2;
        let row = self.row(sample_y as usize);
        for (index, sample) in samples.iter_mut().enumerate() {
            // A band centre is always inside the row for a non-empty width.
            let x = (self.width as usize * (2 * index + 1) / (2 * READBACK_PIXEL_SAMPLES))
                .min(self.width as usize - 1);
            let pixel = &row[x * 4..x * 4 + 4];
            *sample = PixelSample {
                x: u32::try_from(x).unwrap_or(u32::MAX),
                y: sample_y,
                bgra: [pixel[0], pixel[1], pixel[2], pixel[3]],
            };
        }
        samples
    }

    /// The bounded evidence for one validated mapping.
    pub(super) fn readback_sample(&self) -> ReadbackSample {
        ReadbackSample {
            row_pitch: u32::try_from(self.row_pitch).unwrap_or(u32::MAX),
            bytes: self.mapped_bytes,
            valid_row_bytes: self.valid_row_bytes,
            hash_bytes: self.valid_row_bytes * self.height as usize,
            padded: self.row_pitch != self.valid_row_bytes,
            digest: self.digest(),
            samples: self.samples(),
        }
    }
}

/// The device-backed map poll used by production.
struct DeviceMapPoll<'a> {
    context: &'a ID3D11DeviceContext,
    staging: &'a ID3D11Texture2D,
    counters: Arc<Counters>,
    /// The last successful mapping, valid until it is unmapped.
    mapped: D3D11_MAPPED_SUBRESOURCE,
}

impl MapPoll for DeviceMapPoll<'_> {
    fn attempt(&mut self) -> MapAttempt {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the map targets this device's staging texture, the subresource index is
        // valid for a single-mip 2D texture, and DO_NOT_WAIT keeps the call from
        // blocking, so a busy resource is reported instead of waiting inside the driver.
        let result = unsafe {
            self.context.Map(
                self.staging,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        };
        match result {
            Ok(()) => {
                self.mapped = mapped;
                MapAttempt::Mapped
            }
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => MapAttempt::Busy,
            Err(error) => MapAttempt::Failed(map_native_error("ID3D11DeviceContext::Map", error)),
        }
    }

    fn unmap(&mut self) {
        // SAFETY: balances the successful Map recorded in `self.mapped`, exactly once,
        // on the worker thread that created the context.
        unsafe { self.context.Unmap(self.staging, 0) };
        crate::capture::SharedState::bump(&self.counters.readback_unmaps);
    }
}

/// Resolve the full background before any copy or pointer composition. This is
/// descriptor inspection only, never a GPU readback, conversion or CPU wait.
fn frame_background<'a>(
    acquired: &'a ID3D11Texture2D,
    target: &ID3D11Texture2D,
    retained: Option<&'a ID3D11Texture2D>,
) -> Result<(&'a ID3D11Texture2D, bool), RecorderError> {
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    let mut target_desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: live owned/acquired textures; initialized metadata out-parameters.
    unsafe {
        acquired.GetDesc(&mut source_desc);
        target.GetDesc(&mut target_desc);
    }
    let same_layout = |source: &D3D11_TEXTURE2D_DESC| {
        source.Width == target_desc.Width
            && source.Height == target_desc.Height
            && source.MipLevels == target_desc.MipLevels
            && source.ArraySize == target_desc.ArraySize
            && source.SampleDesc.Count == target_desc.SampleDesc.Count
            && source.SampleDesc.Quality == target_desc.SampleDesc.Quality
    };
    if same_layout(&source_desc) && source_desc.Format == target_desc.Format {
        return Ok((acquired, false));
    }
    if same_layout(&source_desc)
        && source_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT
        && target_desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
        && let Some(retained) = retained
    {
        let mut retained_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the caller retains this session-owned cursor-free background.
        unsafe { retained.GetDesc(&mut retained_desc) };
        if same_layout(&retained_desc) && retained_desc.Format == target_desc.Format {
            return Ok((retained, true));
        }
    }
    Err(source_lost_error(format!(
        "DXGI returned an incompatible desktop texture (format {}, {}x{}); the capture slot requires format {}, {}x{}. No current cursor-free pointer-only background is available, so no invalid copy or stale frame was published.",
        source_desc.Format.0,
        source_desc.Width,
        source_desc.Height,
        target_desc.Format.0,
        target_desc.Width,
        target_desc.Height,
    )))
}

/// Draws the frame's cursor into the copied BGRA8 slot when DXGI reports a separate
/// hardware pointer. GDI handles alpha and monochrome/XOR cursor shapes while the
/// desktop image stays entirely on the GPU.
fn draw_cursor_on_surface(
    target: &ID3D11Texture2D,
    cursor: &FramePointer,
    trace: Option<CaptureTrace>,
) -> Result<(), RecorderError> {
    if !cursor.visible() {
        return Ok(());
    }
    let surface: IDXGISurface1 = target
        .cast()
        .map_err(|error| map_native_error("ID3D11Texture2D::cast<IDXGISurface1>", error))?;
    // SAFETY: the target was created with the GDI-compatible flag and is not
    // currently bound to the output merger. `false` preserves the copied pixels.
    let dc = unsafe { surface.GetDC(false) }
        .map_err(|error| map_native_error("IDXGISurface1::GetDC(cursor)", error))?;
    let drawn = cursor.draw(dc);
    cursor_diagnostics::record("dxgi.draw_icon", trace, || {
        serde_json::json!({
            "draw_icon_call_count": 1,
            "draw_icon_success": drawn.is_ok(),
            "software_composition_count": drawn.is_ok().then_some(1),
            "embedded_in_pixels": "unknown",
        })
    });
    // SAFETY: this balances the successful GetDC on every drawing outcome. No D3D
    // command is issued until it returns.
    let released = unsafe { surface.ReleaseDC(None) }
        .map_err(|error| map_native_error("IDXGISurface1::ReleaseDC(cursor)", error));
    drawn?;
    released
}

impl DxgiSource {
    fn normalize_sdr(
        &mut self,
        source: &ID3D11Texture2D,
        slot: usize,
    ) -> Result<(), RecorderError> {
        if self.sdr_converter.is_none() {
            if self.payload_bytes.saturating_add(self.sdr_input_bytes) > MAX_PAYLOAD_BYTES {
                return Err(internal_error(
                    "FP16 SDR input exceeds the capture texture budget.".into(),
                ));
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.backing.width,
                Height: self.backing.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let mut input = None;
            // SAFETY: reserved bounded descriptor on this session's device.
            unsafe {
                self.backing
                    ._device
                    .CreateTexture2D(&desc, None, Some(&mut input))
            }
            .map_err(|e| map_native_error("CreateTexture2D(FP16 SDR input)", e))?;
            let input =
                input.ok_or_else(|| internal_error("FP16 SDR shader input missing.".into()))?;
            let pass = GpuColorPass::new(
                &self.backing._device,
                &input,
                &self.backing.textures,
                ColorDirection::LinearToSdr,
            )?;
            self.sdr_converter = Some((input, pass));
        }
        let (input, pass) = self.sdr_converter.as_ref().expect("prepared SDR converter");
        // SAFETY: the caller validated a same-size FP16 source; commands are
        // ordered before Draw and the existing slot completion query/Flush.
        unsafe { self.context.CopyResource(input, source) };
        pass.draw(&self.context, self.texture(slot)?)
    }

    fn texture(&self, slot_index: usize) -> Result<&ID3D11Texture2D, RecorderError> {
        self.backing.textures.get(slot_index).ok_or_else(|| {
            internal_error(format!(
                "Slot {slot_index} is outside the session's {SLOT_COUNT} preallocated textures."
            ))
        })
    }

    fn query(&self, slot_index: usize) -> Result<&ID3D11Query, RecorderError> {
        self.queries.get(slot_index).ok_or_else(|| {
            internal_error(format!(
                "Slot {slot_index} is outside the session's {SLOT_COUNT} preallocated queries."
            ))
        })
    }
}

impl Drop for DxgiSource {
    fn drop(&mut self) {
        // Bounded cleanup on the worker thread: a frame that was still acquired is
        // released before the duplication object goes away. Errors cannot be reported
        // from Drop, and the duplication's own release releases any remaining frame.
        if self.acquired_outstanding {
            // SAFETY: matches an outstanding successful AcquireNextFrame on this thread.
            let _ = unsafe {
                self.duplication
                    .as_ref()
                    .expect("acquired duplication")
                    .ReleaseFrame()
            };
            self.acquired_outstanding = false;
        }
    }
}

/// Opens the DXGI duplication session for one selected display output.
///
/// Preparation is bounded and cancellable between stages; nothing is left running if
/// it fails. No texture is created before the owned payload has been preflighted.
pub(super) fn open(
    selection: &CaptureOutput,
    preparation: &CapturePreparation<'_>,
    hdr_float: bool,
) -> Result<DxgiSource, RecorderError> {
    let witness = crate::witness::Witness::new()?;
    let cancellation = preparation.cancellation;
    if cancellation.is_cancelled() {
        return Err(super::timing::preparation_cancelled());
    }
    let apartment = ComApartment::initialize_mta()?;
    let scheduling = CaptureScheduling::join();

    let mut frequency = 0i64;
    // SAFETY: the out-parameter is a valid initialized integer.
    unsafe { QueryPerformanceFrequency(&mut frequency) }
        .map_err(|error| map_native_error("QueryPerformanceFrequency", error))?;
    let Some(ticks_per_second) = std::num::NonZeroU64::new(u64::try_from(frequency).unwrap_or(0))
    else {
        return Err(internal_error(
            "QueryPerformanceFrequency reported zero, so native desktop timestamps cannot be mapped."
                .to_string(),
        ));
    };

    if cancellation.is_cancelled() {
        return Err(super::timing::preparation_cancelled());
    }
    // SAFETY: CreateDXGIFactory1 has no preconditions and returns an owned COM object.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_native_error("CreateDXGIFactory1", error))?;

    // DXGI reports output rectangles in the calling thread's DPI coordinate space, so
    // the adapter/output enumeration runs at the same per-monitor-v2 awareness as the
    // display enumeration whose geometry it is checked against.
    let SelectedOutput {
        adapter,
        output,
        adapter_description,
    } = crate::display::with_per_monitor_awareness(|| select_output_handles(&factory, selection))?;
    // This path owns only BGRA8/H.264 SDR surfaces. On an actively HDR output,
    // Desktop Duplication's 8-bit image cannot be mislabeled as preserved HDR.
    // Output6 is unavailable on older Windows builds; those retain the prior SDR path.
    if let Ok(output6) = output.cast::<IDXGIOutput6>() {
        // SAFETY: the selected output remains alive and this is a read-only snapshot.
        let color = unsafe { output6.GetDesc1() }
            .map_err(|error| map_native_error("IDXGIOutput6::GetDesc1", error))?;
        if hdr_float
            && (color.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                || color.BitsPerColor < 10)
        {
            return Err(not_implemented_error("The selected display does not report active PQ/BT.2020 with at least 10 bits per channel.".to_string()));
        }
        if !hdr_float && color.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
            return Err(not_implemented_error(format!(
                "The selected output is actively HDR (PQ/BT.2020, {} bits per channel). The BGRA8/H.264 SDR path has no HDR-preserving conversion or tone map; capture was refused.",
                color.BitsPerColor,
            )));
        }
    } else if hdr_float {
        return Err(not_implemented_error(
            "IDXGIOutput6 is unavailable, so active HDR output cannot be verified.".to_string(),
        ));
    }
    if cancellation.is_cancelled() {
        return Err(super::timing::preparation_cancelled());
    }

    let mut device: Option<ID3D11Device> = None;
    let mut feature_level = D3D_FEATURE_LEVEL(0);
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: the adapter is alive for the call, the driver type is documented as
    // required when an explicit adapter is passed, and the output pointers are valid
    // optionals that the API fills on success. No software device is requested.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
    }
    .map_err(|error| map_native_error("D3D11CreateDevice", error))?;
    let device = device.ok_or_else(|| {
        internal_error("D3D11CreateDevice reported success without a device.".to_string())
    })?;
    let context = context.ok_or_else(|| {
        internal_error(
            "D3D11CreateDevice reported success without an immediate context.".to_string(),
        )
    })?;
    // ADR 0006 R2 addendum: enable and verify D3D11 immediate-context multithread
    // protection before the device is exposed to the encoder or publishes any texture.
    // `SetMultithreadProtected` returns the previous state, not a success flag, so the
    // result is verified with `GetMultithreadProtected`. An unsupported or refused policy
    // is an explicit error rather than a silent single-threaded assumption.
    let multithread: ID3D11Multithread = context.cast().map_err(|error| {
        not_implemented_error(format!(
            "The capture device's immediate context does not expose ID3D11Multithread (HRESULT 0x{:08X}), so this backend cannot enable the required immediate-context protection.",
            error.code().0
        ))
    })?;
    // SAFETY: the interface was queried from this device's live immediate context and the
    // call only sets a device-level policy flag.
    let previous = unsafe { multithread.SetMultithreadProtected(true) };
    // SAFETY: same interface, read-only query.
    if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
        return Err(internal_error(format!(
            "D3D11 immediate-context multithread protection is still disabled after enabling it (previous state {}), so this backend refuses to expose the device to the encoder.",
            previous.as_bool()
        )));
    }
    if feature_level.0 < D3D_FEATURE_LEVEL_11_0.0 {
        return Err(not_implemented_error(format!(
            "The selected display's adapter reports D3D feature level 0x{:X}, and this capture proof requires 11_0 or higher.",
            feature_level.0
        )));
    }

    if cancellation.is_cancelled() {
        return Err(super::timing::preparation_cancelled());
    }
    // Recording preparation owns a device and fixed texture set, but no live
    // duplication buffer yet. Opening duplication early leaves a stale initial
    // snapshot accumulating while the encoder initializes; its first update can
    // then be coalesced even though no application queue has dropped a frame.
    let (duplication, pending_output) = if preparation.defer_frames {
        (None, Some((output, selection.clone(), hdr_float)))
    } else {
        (
            Some(open_duplication(&output, &device, selection, hdr_float)?),
            None,
        )
    };
    let width = selection.width;
    let height = selection.height;

    let query_desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let texture_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: if hdr_float {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: if hdr_float {
            D3D11_BIND_SHADER_RESOURCE.0 as u32
        } else {
            D3D11_BIND_RENDER_TARGET.0 as u32
        },
        CPUAccessFlags: 0,
        MiscFlags: if hdr_float {
            0
        } else {
            D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32
        },
    };
    let mut sink = DeviceSink {
        device: &device,
        surface_desc: texture_desc,
        staging_desc: D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..texture_desc
        },
        query_desc,
        textures: Vec::with_capacity(SLOT_COUNT),
        queries: Vec::with_capacity(SLOT_COUNT),
        staging: None,
    };
    // Preflight, then allocate: a rejected budget creates nothing.
    let plan = if hdr_float {
        let plan = plan_owned_payload_with_bytes(width, height, false, 8)?;
        allocate_owned_payload(plan, &mut sink)?;
        plan
    } else {
        prepare_owned_payload(width, height, preparation.verify_pixels, &mut sink)?
    };
    let DeviceSink {
        textures,
        queries,
        staging,
        ..
    } = sink;

    // One cursor-free copy permits fresh pointer frames while the desktop is static.
    // It is optional at the payload limit so existing large-display support remains.
    let clean_desktop = if preparation.show_cursor
        && !hdr_float
        && plan
            .total_bytes
            .saturating_add(u64::from(width) * u64::from(height) * 4)
            <= MAX_PAYLOAD_BYTES
    {
        let clean_desc = D3D11_TEXTURE2D_DESC {
            BindFlags: 0,
            MiscFlags: 0,
            ..texture_desc
        };
        let mut texture = None;
        // SAFETY: the descriptor is initialized and the device owns the output.
        unsafe { device.CreateTexture2D(&clean_desc, None, Some(&mut texture)) }
            .map_err(|error| map_native_error("CreateTexture2D(clean desktop)", error))?;
        Some(texture.ok_or_else(|| {
            internal_error("CreateTexture2D(clean desktop) returned no texture.".to_string())
        })?)
    } else {
        None
    };

    let description = NativeDescription {
        api: CAPTURE_API.to_string(),
        adapter_description,
        output_gdi_name: selection.adapter_name.clone(),
        width,
        height,
        pixel_format: if hdr_float {
            "DXGI_FORMAT_R16G16B16A16_FLOAT (linear scRGB)".to_string()
        } else {
            PIXEL_FORMAT.to_string()
        },
        rotation_identity: true,
        desktop_image_in_system_memory: duplication.as_ref().is_some_and(|duplication| {
            // SAFETY: live worker-owned duplication, read-only descriptor.
            unsafe { duplication.GetDesc().DesktopImageInSystemMemory.as_bool() }
        }),
        context_multithread_protected: true,
        staging_bytes: plan.staging_bytes,
    };

    // The lease-visible backing holds only the device and the four owned textures. The
    // context, the per-slot queries and the staging texture stay worker-only, so a
    // lease that outlives its session cannot reach the worker's context.
    preparation.lifetime.owner_created();
    let backing = Arc::new(NativeBacking {
        _device: device,
        textures,
        width,
        height,
        pool: preparation.pool_id,
        lifetime: Arc::clone(preparation.lifetime),
        format: if hdr_float {
            SurfaceFormat::ScRgbFloat16
        } else {
            SurfaceFormat::Bgra8
        },
    });

    let payload_bytes = plan.total_bytes
        + if clean_desktop.is_some() {
            u64::from(width) * u64::from(height) * 4
        } else {
            0
        };
    Ok(DxgiSource {
        witness,
        diagnostic_state: CaptureDiagnosticState::new(),
        context,
        queries,
        staging,
        backing,
        description,
        duplication,
        pending_output,
        idle_wait: CaptureIdleWait::new()?,
        ticks_per_second: ticks_per_second.get(),
        acquired_outstanding: false,
        synthetic_outstanding: false,
        clean_desktop,
        clean_desktop_ready: false,
        clean_desktop_separate_pointer: false,
        clean_desktop_present_time: 0,
        latest_desktop_present_time: 0,
        show_cursor: preparation.show_cursor,
        separate_pointer_visible: false,
        pending_pointer_visibility: None,
        pending_desktop_present_time: 0,
        pointer: FramePointer::default(),
        replay_sampling: false,
        replay_sample_after: Instant::now(),
        payload_bytes,
        sdr_input_bytes: if hdr_float {
            0
        } else {
            u64::from(width) * u64::from(height) * 8
        },
        sdr_converter: None,
        last_copied_slot: None,
        cancellation: cancellation.clone(),
        counters: Arc::clone(preparation.counters),
        _scheduling: scheduling,
        _apartment: apartment,
    })
}

/// Activate duplication only when its consumer can process the first image.
/// Geometry/format/topology checks still run before any frame is acquired.
fn open_duplication(
    output: &IDXGIOutput,
    device: &ID3D11Device,
    selection: &CaptureOutput,
    hdr_float: bool,
) -> Result<IDXGIOutputDuplication, RecorderError> {
    // Do the potentially expensive topology enumeration before opening the
    // live duplication stream. The output/device are already selected handles;
    // its native descriptor below still checks activation geometry/rotation.
    let current = crate::display::capture_output(&selection.device_path)?;
    if !current.same_output(selection) {
        return Err(source_lost_error(format!(
            "The selected display {} changed output while capture was prepared, so the session was not started.",
            selection.device_path
        )));
    }
    let output1: IDXGIOutput1 = output.cast().map_err(|error| {
        not_implemented_error(format!(
            "The selected output does not expose IDXGIOutput1, so desktop duplication is unavailable: HRESULT 0x{:08X}.",
            error.code().0
        ))
    })?;
    // SAFETY: the device and output are alive and this worker thread owns them.
    let duplication: IDXGIOutputDuplication = if hdr_float {
        let output5: IDXGIOutput5 = output.cast().map_err(|error| {
            not_implemented_error(format!(
                "IDXGIOutput5::DuplicateOutput1 is unavailable for FP16 capture: HRESULT 0x{:08X}.",
                error.code().0
            ))
        })?;
        // SAFETY: the selected output and its adapter device are alive; the format list
        // requests an FP16 desktop image and is checked again after duplication opens.
        unsafe {
            output5.DuplicateOutput1(
                device,
                0,
                &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM],
            )
        }
        .map_err(|error| map_duplication_error("IDXGIOutput5::DuplicateOutput1", error))?
    } else {
        unsafe { output1.DuplicateOutput(device) }
            .map_err(|error| map_duplication_error("IDXGIOutput1::DuplicateOutput", error))?
    };

    // SAFETY: the duplication object is alive and owned by this worker thread.
    let descriptor: DXGI_OUTDUPL_DESC = unsafe { duplication.GetDesc() };
    if hdr_float && descriptor.ModeDesc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT {
        return Err(not_implemented_error(format!(
            "HDR duplication returned DXGI format {}, not FP16; no 8-bit conversion is accepted.",
            descriptor.ModeDesc.Format.0
        )));
    }
    if descriptor.Rotation != DXGI_MODE_ROTATION_IDENTITY {
        return Err(not_implemented_error(format!(
            "The selected display is rotated (DXGI rotation {}), and this capture proof publishes only identity-rotation surfaces instead of rotated pixels with landscape dimensions.",
            descriptor.Rotation.0
        )));
    }
    if descriptor.ModeDesc.Width != selection.width
        || descriptor.ModeDesc.Height != selection.height
    {
        return Err(source_lost_error(format!(
            "The display changed while capture was prepared: enumeration reported {}x{} and duplication reports {}x{}.",
            selection.width,
            selection.height,
            descriptor.ModeDesc.Width,
            descriptor.ModeDesc.Height
        )));
    }

    Ok(duplication)
}

/// Read-only active output colour query using the same stable display association
/// as capture. `None` means IDXGIOutput6 is unavailable, not an SDR assertion.
pub(super) fn active_hdr(selection: &CaptureOutput) -> Result<Option<bool>, RecorderError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_native_error("CreateDXGIFactory1", error))?;
    let selected =
        crate::display::with_per_monitor_awareness(|| select_output_handles(&factory, selection))?;
    let output6: IDXGIOutput6 = match selected.output.cast() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    let desc = unsafe { output6.GetDesc1() }
        .map_err(|error| map_native_error("IDXGIOutput6::GetDesc1", error))?;
    Ok(Some(
        desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    ))
}

/// One scanned DXGI output, as transient selection evidence only.
///
/// The GDI name, rectangle and monitor handle are compared during selection and never
/// persisted as identity; the stable `DisplayId` remains the only identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputCandidate {
    pub(super) adapter_description: String,
    pub(super) gdi_name: String,
    pub(super) attached: bool,
    /// Full desktop rectangle: left, top, right, bottom.
    pub(super) rect: (i32, i32, i32, i32),
    /// Transient `HMONITOR` value used only for the association check.
    pub(super) monitor: isize,
}

/// One step of an adapter/output scan.
pub(super) enum ScanStep {
    Candidate(OutputCandidate),
    Failed(RecorderError),
    Done,
}

/// Private seam over the adapter/output scan.
///
/// Production drives the real DXGI scan; tests drive a scripted scan, so duplicate
/// detection and scan-failure propagation are covered without a GPU.
pub(super) trait OutputScan {
    fn next(&mut self) -> ScanStep;
}

/// The real scan: every adapter and every output, with the transient handles kept
/// beside the candidates for the chosen index.
struct DxgiScan {
    candidates: Vec<OutputCandidate>,
    handles: Vec<(windows::Win32::Graphics::Dxgi::IDXGIAdapter1, IDXGIOutput)>,
    failure: Option<RecorderError>,
    cursor: usize,
}

impl DxgiScan {
    fn take(
        &mut self,
        index: usize,
    ) -> Option<(windows::Win32::Graphics::Dxgi::IDXGIAdapter1, IDXGIOutput)> {
        self.handles.get(index).cloned()
    }
}

impl OutputScan for DxgiScan {
    fn next(&mut self) -> ScanStep {
        if let Some(candidate) = self.candidates.get(self.cursor) {
            self.cursor += 1;
            return ScanStep::Candidate(candidate.clone());
        }
        match self.failure.take() {
            Some(error) => ScanStep::Failed(error),
            None => ScanStep::Done,
        }
    }
}

/// Scans every adapter and output.
///
/// Only the documented end-of-enumeration `DXGI_ERROR_NOT_FOUND` ends a loop; every
/// other failure is retained and propagated with the API name and HRESULT, and it
/// cannot be mistaken for a completed scan.
fn scan_outputs(factory: &IDXGIFactory1) -> DxgiScan {
    let mut scan = DxgiScan {
        candidates: Vec::new(),
        handles: Vec::new(),
        failure: None,
        cursor: 0,
    };
    let mut adapter_index = 0u32;
    while scan.failure.is_none() {
        // SAFETY: adapter indices are probed until the documented not-found failure.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => {
                scan.failure = Some(map_native_error("IDXGIFactory1::EnumAdapters1", error));
                break;
            }
        };
        // SAFETY: the adapter is alive for the call.
        let adapter_desc = match unsafe { adapter.GetDesc() } {
            Ok(description) => description,
            Err(error) => {
                scan.failure = Some(map_native_error("IDXGIAdapter1::GetDesc", error));
                break;
            }
        };
        let adapter_description = wide_to_string(&adapter_desc.Description);

        let mut output_index = 0u32;
        loop {
            // SAFETY: output indices are probed until the documented not-found failure.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => {
                    scan.failure = Some(map_native_error("IDXGIAdapter::EnumOutputs", error));
                    break;
                }
            };
            // SAFETY: the output is alive for the call.
            let output_desc = match unsafe { output.GetDesc() } {
                Ok(description) => description,
                Err(error) => {
                    scan.failure = Some(map_native_error("IDXGIOutput::GetDesc", error));
                    break;
                }
            };
            scan.candidates.push(OutputCandidate {
                adapter_description: adapter_description.clone(),
                gdi_name: wide_to_string(&output_desc.DeviceName),
                attached: output_desc.AttachedToDesktop.as_bool(),
                rect: (
                    output_desc.DesktopCoordinates.left,
                    output_desc.DesktopCoordinates.top,
                    output_desc.DesktopCoordinates.right,
                    output_desc.DesktopCoordinates.bottom,
                ),
                monitor: output_desc.Monitor.0 as isize,
            });
            scan.handles.push((adapter.clone(), output));
            output_index += 1;
            if scan.candidates.len() > MAX_SCANNED_OUTPUTS {
                scan.failure = Some(internal_error(format!(
                    "More than {MAX_SCANNED_OUTPUTS} attached DXGI outputs were reported, which is outside the bounded selection scan."
                )));
                break;
            }
        }
        adapter_index += 1;
    }
    scan
}

/// Drives a scan to completion and then applies the selection rules.
///
/// The scan is never stopped early: a competing match *and* a scan failure that
/// happens after an initial match are both detected.
pub(super) fn select_output(
    scan: &mut impl OutputScan,
    selection: &CaptureOutput,
) -> Result<(usize, Vec<OutputCandidate>), RecorderError> {
    let mut candidates = Vec::new();
    let mut failure = None;
    loop {
        match scan.next() {
            ScanStep::Candidate(candidate) => candidates.push(candidate),
            ScanStep::Failed(error) => {
                failure = Some(error);
                break;
            }
            ScanStep::Done => break,
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let index = select_candidate(&candidates, selection)?;
    Ok((index, candidates))
}

/// The pure selection rule over scanned candidates.
///
/// Exactly one attached output must match the adapter output the display was
/// enumerated under, with the same full desktop rectangle and the same monitor
/// association. Absent, unattached, mismatched and duplicated candidates are explicit
/// errors; no candidate is ever chosen by elimination.
pub(super) fn select_candidate(
    candidates: &[OutputCandidate],
    selection: &CaptureOutput,
) -> Result<usize, RecorderError> {
    let mut found: Option<usize> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if !candidate
            .gdi_name
            .eq_ignore_ascii_case(&selection.adapter_name)
        {
            continue;
        }
        if found.is_some() {
            return Err(source_lost_error(format!(
                "More than one attached DXGI output reports the name {}, so the capture target is ambiguous.",
                candidate.gdi_name
            )));
        }
        found = Some(index);
    }
    let Some(index) = found else {
        return Err(source_lost_error(format!(
            "No attached DXGI output named {} matches the enumerated display, so the selected display cannot be captured.",
            selection.adapter_name
        )));
    };
    let candidate = &candidates[index];
    if !candidate.attached {
        return Err(source_lost_error(format!(
            "The DXGI output {} exists but is not attached to the desktop, so it cannot be captured.",
            candidate.gdi_name
        )));
    }
    if candidate.rect != selection.desktop_rect {
        return Err(source_lost_error(format!(
            "The DXGI output {} reports rectangle {:?} while display enumeration reported {:?}, so the selected output is no longer the enumerated one.",
            candidate.gdi_name, candidate.rect, selection.desktop_rect
        )));
    }
    if candidate.monitor != selection.monitor_handle {
        return Err(source_lost_error(format!(
            "The DXGI output {} is associated with monitor handle 0x{:X} while display enumeration reported 0x{:X}, so the selected output belongs to a different monitor.",
            candidate.gdi_name, candidate.monitor, selection.monitor_handle
        )));
    }
    Ok(index)
}

/// The resolved adapter/output pair plus its adapter description.
struct SelectedOutput {
    adapter: windows::Win32::Graphics::Dxgi::IDXGIAdapter1,
    output: IDXGIOutput,
    adapter_description: String,
}

/// Runs the real scan, applies the selection rule and returns the transient handles.
fn select_output_handles(
    factory: &IDXGIFactory1,
    selection: &CaptureOutput,
) -> Result<SelectedOutput, RecorderError> {
    let mut scan = scan_outputs(factory);
    let (index, candidates) = select_output(&mut scan, selection)?;
    let (adapter, output) = scan.take(index).ok_or_else(|| {
        internal_error(
            "The selected DXGI output's handles were not retained by the bounded scan.".to_string(),
        )
    })?;
    let adapter_description = candidates
        .get(index)
        .map(|candidate| candidate.adapter_description.clone())
        .unwrap_or_default();
    Ok(SelectedOutput {
        adapter,
        output,
        adapter_description,
    })
}

/// Resolves the same DXGI output as capture start and reports its adapter LUID.
/// Recording policy uses this before opening a session; the encoder checks the
/// first captured lease again, so a topology change cannot select another GPU.
pub(crate) fn display_adapter_identity(
    display_id: &str,
) -> Result<crate::encode::NativeAdapterIdentity, RecorderError> {
    use windows::Win32::Graphics::Dxgi::CreateDXGIFactory1;

    let selection = crate::display::capture_output(display_id)?;
    // SAFETY: the factory owns its COM reference for this short read-only query.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_native_error("CreateDXGIFactory1", error))?;
    let selected =
        crate::display::with_per_monitor_awareness(|| select_output_handles(&factory, &selection))?;
    // SAFETY: selected adapter is retained until its description is copied.
    let desc = unsafe { selected.adapter.GetDesc1() }
        .map_err(|error| map_native_error("IDXGIAdapter1::GetDesc1", error))?;
    let luid =
        u64::from(desc.AdapterLuid.LowPart) | ((desc.AdapterLuid.HighPart as u32 as u64) << 32);
    Ok(crate::encode::NativeAdapterIdentity {
        luid,
        vendor_id: desc.VendorId,
        description: selected.adapter_description,
    })
}

/// The kind of Frame-owned texture being allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextureKind {
    Surface,
    Staging,
}

/// Private allocation seam: the device implementation creates real textures, the unit
/// tests count calls without a GPU.
pub(crate) trait TextureSink {
    fn create(&mut self, kind: TextureKind) -> Result<(), RecorderError>;
}

/// Preflights the owned payload and only then allocates it.
///
/// Both production and the budget test call this function, so "a rejected budget
/// creates nothing" is exercised on the real code path.
pub(crate) fn prepare_owned_payload(
    width: u32,
    height: u32,
    verify_pixels: bool,
    sink: &mut impl TextureSink,
) -> Result<OwnedPayloadPlan, RecorderError> {
    let plan = plan_owned_payload(width, height, verify_pixels)?;
    allocate_owned_payload(plan, sink)?;
    Ok(plan)
}

fn allocate_owned_payload(
    plan: OwnedPayloadPlan,
    sink: &mut impl TextureSink,
) -> Result<(), RecorderError> {
    for _ in 0..plan.slot_count() {
        sink.create(TextureKind::Surface)?;
    }
    if plan.staging_bytes > 0 {
        sink.create(TextureKind::Staging)?;
    }
    Ok(())
}

/// The device-backed sink used by production.
struct DeviceSink<'a> {
    device: &'a ID3D11Device,
    surface_desc: D3D11_TEXTURE2D_DESC,
    staging_desc: D3D11_TEXTURE2D_DESC,
    query_desc: D3D11_QUERY_DESC,
    textures: Vec<ID3D11Texture2D>,
    queries: Vec<ID3D11Query>,
    staging: Option<ID3D11Texture2D>,
}

impl TextureSink for DeviceSink<'_> {
    fn create(&mut self, kind: TextureKind) -> Result<(), RecorderError> {
        let descriptor = match kind {
            TextureKind::Surface => self.surface_desc,
            TextureKind::Staging => self.staging_desc,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: the descriptor is fully initialized and no initial data is passed, so
        // the API only writes into the out-parameter.
        unsafe {
            self.device
                .CreateTexture2D(&descriptor, None, Some(&mut texture))
        }
        .map_err(|error| map_native_error("ID3D11Device::CreateTexture2D", error))?;
        let texture = texture.ok_or_else(|| {
            internal_error("CreateTexture2D reported success without a texture.".to_string())
        })?;
        match kind {
            TextureKind::Surface => {
                // Each surface owns a per-slot event query, so a later poll cannot
                // observe another slot's completion.
                let mut query: Option<ID3D11Query> = None;
                // SAFETY: the descriptor is fully initialized and the API only writes
                // into the out-parameter.
                unsafe { self.device.CreateQuery(&self.query_desc, Some(&mut query)) }
                    .map_err(|error| map_native_error("ID3D11Device::CreateQuery", error))?;
                let query = query.ok_or_else(|| {
                    internal_error("CreateQuery reported success without a query.".to_string())
                })?;
                self.textures.push(texture);
                self.queries.push(query);
            }
            TextureKind::Staging => self.staging = Some(texture),
        }
        Ok(())
    }
}

fn wide_to_string(slice: &[u16]) -> String {
    let length = slice
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..length])
}

/// Maps a native failure onto the accepted error taxonomy, preserving the HRESULT.
fn map_native_error(api: &str, error: windows::core::Error) -> RecorderError {
    let code = error.code();
    let hresult = code.0;
    if code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
    {
        return source_lost_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the desktop duplication or its device is no longer usable."
        ));
    }
    if code == DXGI_ERROR_UNSUPPORTED {
        return not_implemented_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: this adapter or display does not support the requested capture path."
        ));
    }
    if code == E_ACCESSDENIED {
        return permission_denied_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the current desktop session denied access to the display."
        ));
    }
    internal_error(format!("{api} failed with HRESULT 0x{hresult:08X}."))
}

/// Maps a duplication-initialization failure.
///
/// Only documented capability and session conditions are classified; an unexpected
/// failure (including out-of-memory) stays a failure, so a defect can never be
/// reported as unsupported hardware and silently skipped.
fn map_duplication_error(api: &str, error: windows::core::Error) -> RecorderError {
    let code = error.code();
    let hresult = code.0;
    if code == DXGI_ERROR_UNSUPPORTED {
        return not_implemented_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: this adapter or display does not support desktop duplication, and this proof has no WGC or GDI fallback."
        ));
    }
    if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
        return source_lost_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the output is documented as not currently available, for example during a secure-desktop or session transition."
        ));
    }
    map_native_error(api, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::ffi::c_void;

    use std::sync::atomic::Ordering;

    use frame_contracts::ErrorCode;

    use crate::display::CaptureOutput;

    fn selection() -> CaptureOutput {
        CaptureOutput {
            device_path: "\\\\?\\DISPLAY#SYNTHETIC#1&0&UID0#{synthetic}".to_string(),
            adapter_name: "\\\\.\\SYNTHETIC1".to_string(),
            width: 1920,
            height: 1080,
            is_primary: true,
            refresh_rate_hz: Some(60.0),
            desktop_rect: (0, 0, 1920, 1080),
            monitor_handle: 0x1000,
        }
    }

    fn candidate(
        gdi_name: &str,
        attached: bool,
        rect: (i32, i32, i32, i32),
        monitor: isize,
    ) -> OutputCandidate {
        OutputCandidate {
            adapter_description: "Synthetic Adapter".to_string(),
            gdi_name: gdi_name.to_string(),
            attached,
            rect,
            monitor,
        }
    }

    /// A scripted scan: candidates first, then an optional failure, then done.
    struct ScriptedScan {
        steps: VecDeque<ScanStep>,
    }

    impl ScriptedScan {
        fn new(steps: Vec<ScanStep>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
    }

    impl OutputScan for ScriptedScan {
        fn next(&mut self) -> ScanStep {
            self.steps.pop_front().unwrap_or(ScanStep::Done)
        }
    }

    fn step(candidate: OutputCandidate) -> ScanStep {
        ScanStep::Candidate(candidate)
    }

    #[test]
    fn selection_requires_exactly_one_matching_output() {
        let selected = selection();
        let matching = candidate("\\\\.\\SYNTHETIC1", true, (0, 0, 1920, 1080), 0x1000);

        // Absent identity: no candidate matches.
        let mut scan = ScriptedScan::new(vec![step(candidate(
            "\\\\.\\OTHER",
            true,
            (0, 0, 1920, 1080),
            0x1000,
        ))]);
        assert_eq!(
            select_output(&mut scan, &selected).unwrap_err().code,
            ErrorCode::SourceLost
        );

        // Unique match: accepted.
        let mut scan = ScriptedScan::new(vec![
            step(candidate("\\\\.\\OTHER", true, (0, 0, 640, 480), 0x2000)),
            step(matching.clone()),
        ]);
        let (index, candidates) =
            select_output(&mut scan, &selected).expect("one unique match must be selected");
        assert_eq!(index, 1);
        assert_eq!(candidates.len(), 2);

        // Duplicated across adapters/outputs: the scan must not stop at the first match.
        let mut scan = ScriptedScan::new(vec![step(matching.clone()), step(matching.clone())]);
        let error = select_output(&mut scan, &selected)
            .expect_err("two matching outputs are ambiguous, not a unique selection");
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(error.message.contains("ambiguous"), "{}", error.message);

        // Same dimensions but a different position: rejected.
        let mut scan = ScriptedScan::new(vec![step(candidate(
            "\\\\.\\SYNTHETIC1",
            true,
            (0, 1080, 1920, 2160),
            0x1000,
        ))]);
        let error = select_output(&mut scan, &selected)
            .expect_err("a different desktop rectangle must be rejected");
        assert!(error.message.contains("rectangle"), "{}", error.message);

        // Same rectangle but a different monitor association: rejected.
        let mut scan = ScriptedScan::new(vec![step(candidate(
            "\\\\.\\SYNTHETIC1",
            true,
            (0, 0, 1920, 1080),
            0x9999,
        ))]);
        let error = select_output(&mut scan, &selected)
            .expect_err("a different monitor association must be rejected");
        assert!(
            error.message.contains("monitor handle"),
            "{}",
            error.message
        );

        // Detached output: rejected.
        let mut scan = ScriptedScan::new(vec![step(candidate(
            "\\\\.\\SYNTHETIC1",
            false,
            (0, 0, 1920, 1080),
            0x1000,
        ))]);
        let error =
            select_output(&mut scan, &selected).expect_err("a detached output must be rejected");
        assert!(error.message.contains("not attached"), "{}", error.message);
    }

    #[test]
    fn a_scan_failure_after_a_match_is_never_treated_as_a_complete_scan() {
        let selected = selection();
        let matching = candidate("\\\\.\\SYNTHETIC1", true, (0, 0, 1920, 1080), 0x1000);
        let mut scan = ScriptedScan::new(vec![
            step(matching),
            ScanStep::Failed(internal_error(
                "SYNTHETIC scan failure after a match".to_string(),
            )),
        ]);
        let error = select_output(&mut scan, &selected)
            .expect_err("an incomplete scan must be reported, not accepted");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(
            error
                .message
                .contains("SYNTHETIC scan failure after a match")
        );
    }

    /// An allocation sink that counts calls, so "rejected budget allocates nothing"
    /// is checked on the production preparation path.
    #[derive(Default)]
    struct CountingSink {
        surfaces: usize,
        staging: usize,
    }

    impl TextureSink for CountingSink {
        fn create(&mut self, kind: TextureKind) -> Result<(), RecorderError> {
            match kind {
                TextureKind::Surface => self.surfaces += 1,
                TextureKind::Staging => self.staging += 1,
            }
            Ok(())
        }
    }

    #[test]
    fn the_payload_budget_is_rejected_before_any_texture_is_allocated() {
        // Four 7680x4320 BGRA8 surfaces fit on their own.
        let mut sink = CountingSink::default();
        let plan =
            prepare_owned_payload(7680, 4320, false, &mut sink).expect("four slots fit the budget");
        assert_eq!(plan.slots_bytes, 530_841_600);
        assert_eq!(sink.surfaces, SLOT_COUNT);
        assert_eq!(sink.staging, 0);

        // Adding one more surface of the same size (the staging texture) exceeds it:
        // the request must be refused and nothing may be allocated.
        let mut sink = CountingSink::default();
        let error = prepare_owned_payload(7680, 4320, true, &mut sink)
            .expect_err("surfaces plus staging exceed the budget");
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(
            (sink.surfaces, sink.staging),
            (0, 0),
            "a rejected budget must not allocate any texture"
        );

        // A surface set that cannot fit on its own is refused even without staging.
        let mut sink = CountingSink::default();
        assert!(prepare_owned_payload(8192, 4320, false, &mut sink).is_err());
        assert_eq!((sink.surfaces, sink.staging), (0, 0));

        // The reference panel fits with the staging texture included.
        let mut sink = CountingSink::default();
        let plan = prepare_owned_payload(2560, 1600, true, &mut sink)
            .expect("the reference panel fits with staging");
        assert_eq!(plan.staging_bytes, 16_384_000);
        assert_eq!(sink.surfaces, SLOT_COUNT);
        assert_eq!(sink.staging, 1);
    }

    /// Builds a mapped-subresource view over `bytes` for the hashing tests.
    ///
    /// SAFETY: the returned struct borrows `bytes` for its lifetime, which is exactly
    /// the lifetime the mapping rules require.
    fn mapped(bytes: &mut [u8], row_pitch: u32) -> D3D11_MAPPED_SUBRESOURCE {
        D3D11_MAPPED_SUBRESOURCE {
            pData: bytes.as_mut_ptr() as *mut c_void,
            RowPitch: row_pitch,
            DepthPitch: row_pitch * 4,
        }
    }

    /// Builds a validated view over a test buffer and returns its evidence.
    ///
    /// SAFETY: `mapped` borrows `bytes` for `RowPitch * height` initialized bytes,
    /// which is exactly the precondition `MappedView::new` documents, and the view does
    /// not outlive the mapping.
    fn view(
        bytes: &mut [u8],
        row_pitch: u32,
        width: u32,
        height: u32,
    ) -> Result<ReadbackSample, RecorderError> {
        let mapping = mapped(bytes, row_pitch);
        let view = unsafe { MappedView::new(&mapping, width, height) }?;
        Ok(view.readback_sample())
    }

    /// As [`view`], for a mapping that is not backed by a mutable test buffer.
    fn view_raw(
        mapping: &D3D11_MAPPED_SUBRESOURCE,
        width: u32,
        height: u32,
    ) -> Result<ReadbackSample, RecorderError> {
        // SAFETY: the caller built the mapping to describe `width` x `height` pixels.
        let view = unsafe { MappedView::new(mapping, width, height) }?;
        Ok(view.readback_sample())
    }

    /// One scripted map attempt.
    #[derive(Clone, Copy)]
    enum Plan {
        Busy,
        Mapped,
        Fail(&'static str),
    }

    /// A scripted `MapPoll`, so the poll loop, its watchdog, its cancellation checks and
    /// the exactly-once release are exercised without a device.
    struct ScriptedPoll {
        plan: VecDeque<Plan>,
        attempts: usize,
        unmaps: usize,
        /// When set, the poll cancels this token on its first attempt, modelling a
        /// cancellation that arrives while the resource is busy.
        cancel_on_attempt: Option<CaptureCancellation>,
    }

    impl MapPoll for ScriptedPoll {
        fn attempt(&mut self) -> MapAttempt {
            self.attempts += 1;
            if self.attempts == 1
                && let Some(cancellation) = self.cancel_on_attempt.as_ref()
            {
                cancellation.cancel();
            }
            match self.plan.pop_front().unwrap_or(Plan::Busy) {
                Plan::Busy => MapAttempt::Busy,
                Plan::Mapped => MapAttempt::Mapped,
                Plan::Fail(message) => MapAttempt::Failed(internal_error(message.to_string())),
            }
        }

        fn unmap(&mut self) {
            self.unmaps += 1;
        }
    }

    fn plan(steps: Vec<Plan>) -> ScriptedPoll {
        ScriptedPoll {
            plan: steps.into(),
            attempts: 0,
            unmaps: 0,
            cancel_on_attempt: None,
        }
    }

    #[test]
    fn map_poll_retries_while_busy_then_maps_and_releases_once() {
        let counters = Counters::default();
        let cancellation = CaptureCancellation::default();
        let mut poll = plan(vec![Plan::Busy, Plan::Busy, Plan::Mapped]);
        let outcome = poll_until_mapped(
            &mut poll,
            &cancellation,
            &counters,
            Duration::from_millis(500),
        )
        .expect("a busy resource that becomes ready must map");
        assert_eq!(outcome, PollOutcome::Mapped);
        assert_eq!(poll.attempts, 3);
        assert_eq!(counters.readback_map_polls.load(Ordering::Relaxed), 2);
        assert_eq!(poll.unmaps, 0, "no release before the guarded scope");

        // The guard releases exactly once, and only for a successful mapping.
        let released = with_mapping(&mut poll, || 7);
        assert_eq!(released, 7);
        assert_eq!(poll.unmaps, 1);
    }

    #[test]
    fn map_poll_stops_on_cancellation_without_taking_a_mapping() {
        let counters = Counters::default();

        // Cancelled before the first attempt.
        let cancelled = CaptureCancellation::default();
        cancelled.cancel();
        let mut poll = plan(vec![Plan::Mapped]);
        let outcome =
            poll_until_mapped(&mut poll, &cancelled, &counters, Duration::from_millis(10))
                .expect("cancellation is not an error");
        assert_eq!(outcome, PollOutcome::Cancelled);
        assert_eq!(poll.attempts, 0, "cancellation is checked before mapping");
        assert_eq!(poll.unmaps, 0);

        // Cancelled between polls, because the busy attempt requested cancellation.
        let between = CaptureCancellation::default();
        let mut poll = plan(vec![Plan::Busy, Plan::Mapped]);
        poll.cancel_on_attempt = Some(between.clone());
        let outcome = poll_until_mapped(&mut poll, &between, &counters, Duration::from_millis(500))
            .expect("a cancellation between polls is not an error");
        assert_eq!(outcome, PollOutcome::Cancelled);
        assert_eq!(poll.attempts, 1);
        assert_eq!(poll.unmaps, 0, "a cancelled poll must not release anything");
    }

    #[test]
    fn map_poll_watchdog_expiry_is_an_explicit_failure() {
        let counters = Counters::default();
        let cancellation = CaptureCancellation::default();
        let mut poll = plan(vec![Plan::Busy]);
        let error = poll_until_mapped(
            &mut poll,
            &cancellation,
            &counters,
            Duration::from_millis(3),
        )
        .expect_err("an always-busy resource must hit the watchdog");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("watchdog"), "{}", error.message);
        assert!(poll.attempts >= 2, "the poll must retry before expiring");
        assert_eq!(
            poll.unmaps, 0,
            "a watchdog expiry must not release anything"
        );
    }

    #[test]
    fn map_poll_native_error_is_returned_and_releases_nothing() {
        let counters = Counters::default();
        let cancellation = CaptureCancellation::default();
        let mut poll = plan(vec![
            Plan::Busy,
            Plan::Fail("SYNTHETIC map failure"),
            Plan::Mapped,
        ]);
        let error = poll_until_mapped(
            &mut poll,
            &cancellation,
            &counters,
            Duration::from_millis(500),
        )
        .expect_err("a native map failure must be returned");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("SYNTHETIC map failure"));
        assert_eq!(poll.unmaps, 0);
    }

    #[test]
    fn a_mapping_is_released_exactly_once_on_a_validation_error() {
        let counters = Counters::default();
        let cancellation = CaptureCancellation::default();
        let mut poll = plan(vec![Plan::Busy, Plan::Mapped]);
        let outcome = poll_until_mapped(
            &mut poll,
            &cancellation,
            &counters,
            Duration::from_millis(500),
        )
        .expect("the resource becomes ready");
        assert_eq!(outcome, PollOutcome::Mapped);

        // Inside the guarded scope a malformed layout is rejected, and the mapping is
        // still released exactly once.
        let mapping = D3D11_MAPPED_SUBRESOURCE {
            pData: std::ptr::null_mut(),
            RowPitch: 4,
            DepthPitch: 8,
        };
        let error = with_mapping(&mut poll, || {
            // SAFETY: the mapping is presented as a null pointer, so `MappedView::new`
            // must reject it before dereferencing anything.
            match unsafe { MappedView::new(&mapping, 4, 2) } {
                Ok(_) => panic!("a null mapping must be rejected"),
                Err(error) => error,
            }
        });
        assert!(error.message.contains("null pointer"));
        assert_eq!(
            poll.unmaps, 1,
            "validation failure still releases the mapping"
        );
    }

    #[test]
    fn a_mapping_is_released_exactly_once_when_the_body_unwinds() {
        let mut poll = plan(vec![Plan::Mapped]);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_mapping(&mut poll, || panic!("SYNTHETIC readback unwind"))
        }));
        assert!(unwound.is_err(), "the injected panic must propagate");
        assert_eq!(
            poll.unmaps, 1,
            "an unwinding body must still release the mapping exactly once"
        );

        // A second run with a normal body proves the release is not duplicated.
        let mut poll = plan(vec![Plan::Mapped]);
        let value = with_mapping(&mut poll, || 41);
        assert_eq!(value, 41);
        assert_eq!(poll.unmaps, 1);
    }
    #[test]
    fn readback_borrows_only_initialized_pixels_between_uninitialized_padding() {
        let mut storage = [std::mem::MaybeUninit::<u8>::uninit(); 12];
        for (offset, pixel) in [(0, [0, 0, 255, 255]), (8, [0, 255, 0, 255])] {
            for (index, byte) in pixel.into_iter().enumerate() {
                storage[offset + index].write(byte);
            }
        }
        let mapping = D3D11_MAPPED_SUBRESOURCE {
            pData: storage.as_mut_ptr().cast(),
            RowPitch: 8,
            DepthPitch: 0,
        };
        // SAFETY: two initialized four-byte pixel rows at offsets 0 and 8 in one
        // allocation, kept live and unchanged throughout the view. Padding is uninitialized.
        let padded = unsafe { MappedView::new(&mapping, 1, 2) }.unwrap();
        let mut tight = [0, 0, 255, 255, 0, 255, 0, 255];
        let expected = view(&mut tight, 4, 1, 2).unwrap();
        assert_eq!(padded.digest(), expected.digest);
        assert_eq!(padded.samples()[0].bgra, [0, 255, 0, 255]);
    }

    #[test]
    fn readback_rejects_spans_larger_than_isize_before_pointer_access() {
        let mapping = D3D11_MAPPED_SUBRESOURCE {
            pData: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
            RowPitch: u32::MAX,
            DepthPitch: 0,
        };
        // SAFETY: the oversized layout must be rejected before any pointer access.
        let result = unsafe { MappedView::new(&mapping, 1, u32::MAX) };
        assert!(result.is_err());
    }

    #[test]
    fn readback_hashes_every_valid_channel_byte_and_ignores_row_padding() {
        // Two rows of four BGRA pixels: row 0 red, row 1 green.
        let mut tight = Vec::new();
        for row in 0..2u8 {
            for pixel in 0..4u32 {
                let _ = pixel;
                tight.extend_from_slice(if row == 0 {
                    &[0, 0, 255, 255]
                } else {
                    &[0, 255, 0, 255]
                });
            }
        }
        let mut padded = Vec::new();
        for row in 0..2u8 {
            for _ in 0..4 {
                padded.extend_from_slice(if row == 0 {
                    &[0, 0, 255, 255]
                } else {
                    &[0, 255, 0, 255]
                });
            }
            // Padding bytes differ between rows: they are stride, not pixels.
            padded.extend_from_slice(&[row, row, row, row]);
        }

        let tight_sample = view(&mut tight, 16, 4, 2).expect("tight layout");
        let padded_sample = view(&mut padded, 20, 4, 2).expect("padded layout");
        assert_eq!(
            tight_sample.digest, padded_sample.digest,
            "row padding must not change the digest"
        );
        assert_eq!(tight_sample.valid_row_bytes, 16);
        assert_eq!(tight_sample.hash_bytes, 32);
        assert_eq!(tight_sample.bytes, 32);
        assert_eq!(padded_sample.bytes, 40);
        assert!(padded_sample.padded);
        assert!(!tight_sample.padded);
    }

    #[test]
    fn readback_detects_single_channel_changes() {
        let mut red = Vec::new();
        let mut green_only = Vec::new();
        let mut red_only = Vec::new();
        for _ in 0..16 {
            red.extend_from_slice(&[0, 0, 200, 255]);
            // Only the green channel differs from `red`.
            green_only.extend_from_slice(&[0, 40, 200, 255]);
            // Only the red channel differs from `red`.
            red_only.extend_from_slice(&[0, 0, 201, 255]);
        }
        let red_digest = view(&mut red, 64, 16, 1).expect("layout").digest;
        let green_digest = view(&mut green_only, 64, 16, 1).expect("layout").digest;
        let red_only_digest = view(&mut red_only, 64, 16, 1).expect("layout").digest;
        assert_ne!(
            red_digest, green_digest,
            "a green-only change must be visible"
        );
        assert_ne!(
            red_digest, red_only_digest,
            "a red-only change must be visible"
        );
    }

    #[test]
    fn readback_rejects_malformed_layouts_and_samples_the_expected_coordinates() {
        let mut bytes = vec![0u8; 32];
        // 4x2 with a 16-byte row: valid.
        let sample = view(&mut bytes, 16, 4, 2).expect("valid layout");
        assert_eq!(sample.samples.len(), READBACK_PIXEL_SAMPLES);
        for (index, point) in sample.samples.iter().enumerate() {
            // One point per equal-width band at mid-height, inside the image.
            assert!(point.x < 4, "sample {index} must be inside the row");
            assert_eq!(point.y, 1, "samples are taken at mid-height");
        }

        // A row pitch smaller than the valid pixel row cannot be addressed safely.
        let error = view(&mut bytes, 8, 4, 2).expect_err("a short row pitch must be rejected");
        assert!(error.message.contains("smaller than"), "{}", error.message);

        // Zero-sized surfaces have no pixels to sample and must be rejected before any
        // row arithmetic, so sampling cannot panic.
        let mut empty = vec![0u8; 16];
        for (width, height) in [(0u32, 2u32), (4, 0), (0, 0)] {
            let error = view(&mut empty, 16, width, height)
                .expect_err("a zero-sized surface must be rejected");
            assert!(error.message.contains("no pixels"), "{}", error.message);
        }

        // The borrow covers only the bytes it touches: the final row's trailing padding
        // is outside the validated span.
        let mut rows = vec![0u8; 40];
        let mapping = mapped(&mut rows, 20);
        // SAFETY: the mapping describes 4 x 2 pixels with a 20-byte row pitch, so the
        // valid rows span 2 * 20 - 4 = 36 bytes and are initialized.
        let tight = unsafe { MappedView::new(&mapping, 4, 2) }.expect("valid layout");
        assert_eq!(tight.readback_sample().bytes, 40);
        assert_eq!(tight.readback_sample().hash_bytes, 32);
        assert_eq!(
            tight.digest(),
            view(&mut rows, 20, 4, 2).expect("valid").digest
        );

        // A null mapping pointer is rejected rather than dereferenced.
        let null_mapping = D3D11_MAPPED_SUBRESOURCE {
            pData: std::ptr::null_mut(),
            RowPitch: 16,
            DepthPitch: 32,
        };
        let error = view_raw(&null_mapping, 4, 2).expect_err("a null mapping must be rejected");
        assert!(error.message.contains("null pointer"));
    }
}
