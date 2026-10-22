//! Windows.Graphics.Capture and D3D11 native window session (FRAME-0204).
//!
//! This is the only module in the window capture path that performs native calls, and it
//! is part of the reviewed FFI boundary of the crate's `unsafe_code = "deny"` policy.
//! Every `unsafe` block carries a SAFETY comment. Only official bindings are used: the
//! `windows` crate for WinRT/Direct3D and the `windows-sys` crate for the Win32 window
//! checks. There are no handwritten vtables, no manual ABI, no injection and no fallback
//! to GDI, a display crop or a magnifier presented as window capture.
//!
//! Native behavior this module relies on:
//!
//! - `CreateForWindow` binds to the selected top-level HWND. Child selection uses
//!   the exact child's physical bounds within its verified root's WGC surface.
//! - Resizes recreate the WGC pool after submitted source frames retire. Output
//!   surfaces keep the initial encoder canvas; GPU aspect fit preserves all content.
//! - `Direct3D11CaptureFramePool::CreateFreeThreaded` needs no dispatcher or message
//!   pump. Its arrival callback only signals an event; the worker retrieves frames.
//! - `TryGetNextFrame` reports the documented "no frame available" as an *empty* error
//!   (HRESULT `S_OK` with no message), because the projection turns the documented null
//!   frame into an empty error. Every non-empty HRESULT is a real failure.
//! - A closed window is **not** reported by the frame pool: `TryGetNextFrame` keeps
//!   answering "no frame". Closure and handle reuse are therefore detected by
//!   re-verifying the window identity on every poll, which is why this module does it
//!   before every frame poll.
//! - `Direct3D11CaptureFrame::SystemRelativeTime` is in 100 ns units on the same counter
//!   as `QueryPerformanceCounter`; the conversion is applied explicitly and the result is
//!   measured against the counter on the first frame of every session rather than assumed.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use frame_contracts::RecorderError;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, HANDLE, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Query, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
    IDXGIKeyedMutex,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, Interface};

use super::{
    CapturePolicyEvidence, MAX_FRAME_AGE, MAX_FRAME_LEAD, TimestampBaseEvidence,
    WINDOW_FRAME_POOL_BUFFERS,
};
use crate::capture::CapturePreparation;
use crate::capture::diagnostics::NativeDescription;
use crate::capture::native::{ComApartment, TextureKind, TextureSink, prepare_owned_payload};
use crate::capture::timing::preparation_cancelled;
use crate::capture::worker::{
    Acquired, CaptureSource, FrameObservation, ReadbackOutcome, internal_error,
    merge_release_failure, not_implemented_error, permission_denied_error, source_lost_error,
};
use crate::cursor_preview_diagnostics::{self as cursor_diagnostics, CaptureTrace};
use crate::surface::{GpuSurfaceSet, SLOT_COUNT, SharedPayload};
use crate::window_discovery::WindowIdentity;

/// The API this backend uses, recorded in diagnostics.
const CAPTURE_API: &str = "Windows.Graphics.Capture (IGraphicsCaptureItemInterop::CreateForWindow + Direct3D11CaptureFramePool::CreateFreeThreaded) + D3D11";
/// The image format this backend publishes.
const PIXEL_FORMAT: &str = "DXGI_FORMAT_B8G8R8A8_UNORM";
/// The vendor this backend prefers for its device, so the captured texture is directly
/// encodable by the accepted hardware encoder, which is device-bound. The choice is
/// reported in diagnostics; there is no silent second path.
const NVIDIA_VENDOR_ID: u32 = 0x10DE;
/// Bound on the adapter scan, so a broken driver cannot make selection unbounded.
const MAX_SCANNED_ADAPTERS: usize = 16;
/// Units the capture API reports frame times in (`SystemRelativeTime` is 100 ns).
const FRAME_UNITS_PER_SECOND: u64 = 10_000_000;
/// Longest single wait inside one `acquire` call, whatever bound the caller passes.
const MAX_POLL_WAIT_MS: u32 = 50;
/// Shutdown waits for submitted GPU copies before returning their source frames to
/// the WGC pool. A wedged device must not hold the worker indefinitely.
const COPY_RETIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The capture frame pool's format and the WinRT device cast.
///
/// The source keeps each acquired frame until its copy completes (or closes it
/// immediately if no copy was submitted), so the worker's image handle carries no data.
pub(super) type Image = ();

struct WindowDiagnosticState {
    source_id: u64,
    sequence: u64,
    current: Option<CaptureTrace>,
    copied_slot: Option<usize>,
    cursor_capture_enabled: bool,
}

impl WindowDiagnosticState {
    fn new(cursor_capture_enabled: bool) -> Option<Self> {
        cursor_diagnostics::enabled().then(|| Self {
            source_id: cursor_diagnostics::new_source_id(),
            sequence: 0,
            current: None,
            copied_slot: None,
            cursor_capture_enabled,
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
        self.copied_slot = None;
        trace
    }
}

/// Owns checked-out pool frames while their asynchronous copies read them.
/// `release_frame` moves a submitted frame here; only the matching completed
/// query returns it for Close.
struct SubmittedFrameHolds<T> {
    submitted_slot: Option<usize>,
    in_flight: Vec<Option<T>>,
}

impl<T> SubmittedFrameHolds<T> {
    fn new() -> Self {
        Self {
            submitted_slot: None,
            in_flight: (0..SLOT_COUNT).map(|_| None).collect(),
        }
    }

    fn mark_submitted(&mut self, slot_index: usize) -> Result<(), RecorderError> {
        if self.submitted_slot.is_some() || !matches!(self.in_flight.get(slot_index), Some(None)) {
            return Err(internal_error(format!(
                "Capture slot {slot_index} already owns a submitted frame or is invalid."
            )));
        }
        self.submitted_slot = Some(slot_index);
        Ok(())
    }

    /// Returns the frame only when no copy was submitted.
    fn release(&mut self, frame: T) -> Option<T> {
        if let Some(slot_index) = self.submitted_slot.take() {
            self.in_flight[slot_index] = Some(frame);
            None
        } else {
            Some(frame)
        }
    }

    fn take_completed(&mut self, slot_index: usize) -> Result<T, RecorderError> {
        self.in_flight
            .get_mut(slot_index)
            .and_then(Option::take)
            .ok_or_else(|| {
                internal_error(format!(
                    "Capture slot {slot_index} completed without an owned source frame."
                ))
            })
    }
}

/// The event is shared with the registered callback. Unregistering a WinRT event does
/// not establish that a callback already in progress has returned, so its last callback
/// reference, rather than the worker's teardown order, closes the Win32 handle.
struct FrameArrivalEvent(isize);

impl FrameArrivalEvent {
    fn create() -> Result<Arc<Self>, RecorderError> {
        // SAFETY: the event is created unsignaled and auto-reset with default security.
        let handle = unsafe { CreateEventW(None, false, false, windows::core::PCWSTR::null()) }
            .map_err(|error| map_capture_error("CreateEventW", error))?;
        Ok(Arc::new(Self(handle.0 as isize)))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}

impl Drop for FrameArrivalEvent {
    fn drop(&mut self) {
        // SAFETY: this is the last Arc reference to this one owned handle. A callback
        // retains its own Arc for its full invocation, including its SetEvent call.
        let _ = unsafe { CloseHandle(self.handle()) };
    }
}

/// Owns every capture object from creation onward, including preparation failures.
/// Close is explicit while the worker's MTA and D3D context are still alive; Drop is
/// the safety net for any early return or unwind during preparation.
struct CaptureRuntime {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    frame_arrived_event: Option<Arc<FrameArrivalEvent>>,
    frame_arrived_token: Option<i64>,
    frame_arrived_handler:
        Option<TypedEventHandler<Direct3D11CaptureFramePool, windows::core::IInspectable>>,
    closed: bool,
}

impl CaptureRuntime {
    fn new(pool: Direct3D11CaptureFramePool, session: GraphicsCaptureSession) -> Self {
        Self {
            session,
            pool,
            frame_arrived_event: None,
            frame_arrived_token: None,
            frame_arrived_handler: None,
            closed: false,
        }
    }

    fn register_arrival(&mut self) -> Result<(), RecorderError> {
        let event = FrameArrivalEvent::create()?;
        let callback_event = Arc::clone(&event);
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
                move |_sender, _args| {
                    // SAFETY: the callback owns an Arc to the event for this whole call.
                    unsafe { SetEvent(callback_event.handle()) }
                },
            );
        let token = self.pool.FrameArrived(&handler).map_err(|error| {
            map_capture_error("Direct3D11CaptureFramePool::FrameArrived", error)
        })?;
        self.frame_arrived_event = Some(event);
        self.frame_arrived_token = Some(token);
        self.frame_arrived_handler = Some(handler);
        Ok(())
    }

    fn arrival_event(&self) -> Result<HANDLE, RecorderError> {
        self.frame_arrived_event
            .as_ref()
            .map(|event| event.handle())
            .ok_or_else(|| {
                internal_error("The capture frame-arrival handler is not registered.".to_string())
            })
    }

    fn close_checked(&mut self) -> Result<(), RecorderError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut failure = None;
        if let Some(token) = self.frame_arrived_token.take()
            && let Err(error) = self.pool.RemoveFrameArrived(token)
        {
            failure = Some(map_capture_error(
                "Direct3D11CaptureFramePool::RemoveFrameArrived",
                error,
            ));
        }
        // Keep our handler and event references through both native closes. A callback
        // already executing holds its own event reference even after these are released.
        if let Err(error) = self.session.Close() {
            failure
                .get_or_insert_with(|| map_capture_error("GraphicsCaptureSession::Close", error));
        }
        if let Err(error) = self.pool.Close() {
            failure.get_or_insert_with(|| {
                map_capture_error("Direct3D11CaptureFramePool::Close", error)
            });
        }
        self.frame_arrived_handler = None;
        self.frame_arrived_event = None;
        failure.map_or(Ok(()), Err)
    }

    fn close(&mut self) {
        let _ = self.close_checked();
    }
}

fn preparation_failure(capture: &mut CaptureRuntime, primary: RecorderError) -> RecorderError {
    match capture.close_checked() {
        Ok(()) => primary,
        Err(cleanup) => merge_release_failure(Some(primary), cleanup),
    }
}

impl Drop for CaptureRuntime {
    fn drop(&mut self) {
        self.close();
    }
}

/// The Windows.Graphics.Capture window source. Created on the worker thread and used only
/// there.
pub(super) struct WindowSource {
    witness: crate::witness::Witness,
    diagnostic_state: Option<WindowDiagnosticState>,
    /// Worker-only resources: the immediate context and this session's per-slot
    /// completion queries. They die with the worker and are never reachable from a lease.
    context: ID3D11DeviceContext,
    queries: Vec<ID3D11Query>,
    backing: Arc<GpuSurfaceSet>,
    description: NativeDescription,
    /// The verified identity, re-checked before every frame poll and after item creation.
    identity: WindowIdentity,
    capture_identity: WindowIdentity,
    /// Held for ownership: the capture item keeps this session bound to the window whose
    /// lifetime it describes.
    _item: GraphicsCaptureItem,
    /// Pool, session, frame-arrival subscription and event have one teardown path.
    capture: CaptureRuntime,
    winrt_device: IDirect3DDevice,
    /// The acquired frame, held here until `release_frame` transfers or closes it.
    current: Option<Direct3D11CaptureFrame>,
    acquired_outstanding: bool,
    /// A WGC frame must remain checked out until the GPU has finished reading it.
    submitted_frames: SubmittedFrameHolds<Direct3D11CaptureFrame>,
    width: u32,
    height: u32,
    output_width: u32,
    output_height: u32,
    ticks_per_second: u64,
    timestamp_base: Arc<OnceLock<TimestampBaseEvidence>>,
    /// Recreate only after every submitted WGC frame has retired. Published output
    /// leases, encoder dimensions and the capture session remain unchanged.
    pending_resize: Option<windows::Graphics::SizeInt32>,
    region: D3D11_BOX,
    fit: Option<crate::compositor::fit::TextureFit>,
    fit_error: Option<RecorderError>,
    /// Transition/hidden frames this session refused to publish.
    /// Window-specific evidence: the shared capture counters have no equivalent.
    unpublishable_updates: Arc<AtomicU64>,
    /// Dropped last, so this thread's COM apartment is uninitialized after the session's
    /// own references.
    _apartment: ComApartment,
}

impl CaptureSource for WindowSource {
    type Image = Image;

    fn acquire(&mut self, timeout_ms: u32) -> Result<Option<Acquired<Self::Image>>, RecorderError> {
        if self.acquired_outstanding {
            return Err(internal_error(
                "A capture frame was acquired before the previously acquired one was released."
                    .to_string(),
            ));
        }
        // The frame pool does not report a closed window (measured: it keeps answering
        // "no frame"), so the identity is re-verified on every poll and a lost source is
        // reported as such instead of looking like an idle window forever. This check
        // reads only the window manager: the DWM cloak state is a start-time admission
        // rule and is deliberately not part of the per-poll path.
        let state = self.identity.verify()?;
        let root_state = if self.identity == self.capture_identity {
            state
        } else {
            self.capture_identity.verify()?
        };
        if self.identity != self.capture_identity
            && self.identity.capture_root()? != self.capture_identity
        {
            return Err(source_lost_error(
                "The selected child window was reparented; select it again.".into(),
            ));
        }
        if state.minimized || !state.visible || root_state.minimized || !root_state.visible {
            self.pending_resize = None;
        } else if let Some(size) = self.pending_resize {
            if self.submitted_frames.in_flight.iter().any(Option::is_some) {
                return Ok(None);
            }
            // Recreate discards queued frames. No current or GPU-owned frame may
            // still refer to the old pool when this happens.
            crate::surface::plan_owned_payload(size.Width as u32, size.Height as u32, false)?;
            self.capture
                .pool
                .Recreate(
                    &self.winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    WINDOW_FRAME_POOL_BUFFERS,
                    size,
                )
                .map_err(|e| map_capture_error("Direct3D11CaptureFramePool::Recreate", e))?;
            self.width = size.Width as u32;
            self.height = size.Height as u32;
            self.pending_resize = None;
        }

        // Wait for the pool's own frame-arrival signal instead of polling it: the pool
        // raises that signal from its delivery thread, and a bounded wait is the
        // documented way to find out that a frame is ready. The bound is the worker's own
        // acquire timeout, so an idle window still produces the same bounded "no update"
        // answer.
        // SAFETY: the source owns the event for the duration of this bounded wait.
        match unsafe {
            WaitForSingleObject(
                self.capture.arrival_event()?,
                timeout_ms.min(MAX_POLL_WAIT_MS),
            )
        } {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => {}
            other => {
                return Err(internal_error(format!(
                    "Waiting for the capture frame-arrival signal failed with 0x{:08X}.",
                    other.0
                )));
            }
        }

        // SAFETY: the free-threaded frame pool was created by this worker thread's device
        // and needs no dispatcher; the call only returns an owned frame or the documented
        // empty result.
        let phase = self.witness.enter(c"window/TryGetNextFrame");
        let result = self.capture.pool.TryGetNextFrame();
        drop(phase);
        let frame = match result {
            Ok(frame) => frame,
            // The projection reports the documented "no frame available" as an empty
            // error; an empty HRESULT is that answer, never a success.
            Err(error) if error.code().0 == 0 => {
                return Ok(None);
            }
            Err(error) => {
                return Err(map_capture_error(
                    "Direct3D11CaptureFramePool::TryGetNextFrame",
                    error,
                ));
            }
        };

        let trace = self
            .diagnostic_state
            .as_mut()
            .map(WindowDiagnosticState::acquired);
        cursor_diagnostics::record("wgc.frame_received", trace, || {
            serde_json::json!({
                "cursor_capture_enabled": self.diagnostic_state.as_ref().map(|state| state.cursor_capture_enabled),
                "software_composition_count": 0,
                "embedded_in_pixels": "unknown",
                "frame_pool_width": self.width,
                "frame_pool_height": self.height,
            })
        });
        let observation = match self.observe(&frame) {
            Ok(Some(observation)) => observation,
            // The frame carries no publishable image for this session's fixed geometry.
            // It is released and counted here, and this acquire call reports "no update"
            // like any other bounded empty poll.
            Ok(None) => {
                cursor_diagnostics::record("wgc.frame_rejected", trace, || {
                    serde_json::json!({
                        "reason": "unpublishable_geometry_or_visibility",
                        "software_composition_count": 0,
                    })
                });
                self.current = Some(frame);
                self.acquired_outstanding = true;
                self.release_frame()?;
                return Ok(None);
            }
            Err(error) => {
                cursor_diagnostics::record("wgc.frame_error", trace, || {
                    serde_json::json!({
                        "error": error.message, "software_composition_count": 0,
                    })
                });
                // The acquired frame is released here so a rejected frame cannot leak and
                // the failure stays the primary one.
                self.current = Some(frame);
                self.acquired_outstanding = true;
                let released = self.release_frame();
                return Err(match released {
                    Ok(()) => error,
                    Err(release) => merge_release_failure(Some(error), release),
                });
            }
        };
        self.current = Some(frame);
        self.acquired_outstanding = true;
        Ok(Some(Acquired {
            image: (),
            observation,
        }))
    }

    fn begin_copy(&mut self, _image: &Self::Image, slot_index: usize) -> Result<(), RecorderError> {
        let frame = self.current.as_ref().ok_or_else(|| {
            internal_error("A copy was requested without an acquired capture frame.".to_string())
        })?;
        // SAFETY: the frame is alive for the call and belongs to this worker's pool.
        let surface = frame
            .Surface()
            .map_err(|error| map_capture_error("Direct3D11CaptureFrame::Surface", error))?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|error| {
            internal_error(format!(
                "The capture frame's surface does not expose IDirect3DDxgiInterfaceAccess: HRESULT 0x{:08X}.",
                error.code().0
            ))
        })?;
        // SAFETY: the interface was queried from a live capture frame surface and the
        // requested interface is the D3D11 texture that surface documents itself as.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|error| {
            internal_error(format!(
                "The capture frame's surface did not provide an ID3D11Texture2D: HRESULT 0x{:08X}.",
                error.code().0
            ))
        })?;
        let target = self.texture(slot_index)?.clone();
        let query = self.query(slot_index)?.clone();
        let trace = self
            .diagnostic_state
            .as_ref()
            .and_then(|state| state.current);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let region = self.region;
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || region.right > desc.Width
            || region.bottom > desc.Height
        {
            return Err(source_lost_error(
                "WGC returned an incompatible surface during a window transition.".into(),
            ));
        }
        let size = (region.right - region.left, region.bottom - region.top);
        // Preserve the existing even-edge copy for native geometry. Other sizes
        // fit the whole selected source into the original recording canvas.
        let direct = size.0 & !1 == self.output_width && size.1 & !1 == self.output_height;
        if !direct {
            let fit = self.fit.as_mut().ok_or_else(|| {
                self.fit_error.clone().unwrap_or_else(|| {
                    not_implemented_error(
                        "This adapter cannot fit resized window capture on the GPU.".into(),
                    )
                })
            })?;
            fit.prepare(size)?;
        }
        // Nothing after this mark can fail before release_frame transfers the frame.
        self.submitted_frames.mark_submitted(slot_index)?;
        // SAFETY: both textures were created by the device owned here, the copy is queued
        // on this worker's immediate context, and the query is per-slot so a later poll
        // cannot observe another slot's completion.
        let copy_started_ns = trace.map(|_| cursor_diagnostics::now_ns());
        let copy_finished_ns;
        let flush_started_ns;
        let flush_finished_ns;
        unsafe {
            let phase = self.witness.enter(c"window/CopyToEncodeSurface");
            if !direct {
                self.fit.as_ref().expect("fit was prepared").draw(
                    &self.context,
                    &texture,
                    region,
                    slot_index,
                );
            } else if region.left == 0
                && region.top == 0
                && desc.Width == self.output_width
                && desc.Height == self.output_height
            {
                self.context.CopyResource(&target, &texture);
            } else {
                let crop = D3D11_BOX {
                    left: region.left,
                    top: region.top,
                    front: 0,
                    right: region.left + self.output_width,
                    bottom: region.top + self.output_height,
                    back: 1,
                };
                self.context
                    .CopySubresourceRegion(&target, 0, 0, 0, 0, &texture, 0, Some(&crop));
            }
            drop(phase);
            copy_finished_ns = trace.map(|_| cursor_diagnostics::now_ns());
            flush_started_ns = trace.map(|_| cursor_diagnostics::now_ns());
            let phase = self.witness.enter(c"window/EndQuery");
            self.context.End(&query);
            drop(phase);
            let _phase = self.witness.enter(c"window/Flush");
            self.context.Flush();
            flush_finished_ns = trace.map(|_| cursor_diagnostics::now_ns());
        }
        if let Some(state) = self.diagnostic_state.as_mut() {
            state.copied_slot = Some(slot_index);
        }
        cursor_diagnostics::record("wgc.copy_submitted", trace, || {
            serde_json::json!({
                "slot": slot_index,
                "direct_copy": direct,
                "region_left": region.left,
                "region_top": region.top,
                "region_right": region.right,
                "region_bottom": region.bottom,
                "output_width": self.output_width,
                "output_height": self.output_height,
                "copy_commands_cpu_ns": copy_started_ns.zip(copy_finished_ns).map(|(start,end)| end.saturating_sub(start)),
                "flush_commands_cpu_ns": flush_started_ns.zip(flush_finished_ns).map(|(start,end)| end.saturating_sub(start)),
                "cursor_capture_enabled": self.diagnostic_state.as_ref().map(|state| state.cursor_capture_enabled),
                "software_composition_count": 0,
                "embedded_in_pixels": "unknown",
            })
        });
        // CopyResource is asynchronous. Keep the WGC frame checked out until this
        // slot's completion query reports that the GPU has finished reading it.
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
            let _phase = self.witness.enter(c"window/GetData");
            self.context.GetData(
                query,
                Some(&mut complete as *mut BOOL as *mut c_void),
                std::mem::size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
        };
        let complete = match result {
            Ok(()) => complete.as_bool(),
            Err(error) => {
                return Err(map_capture_error(
                    "ID3D11DeviceContext::GetData(D3D11_QUERY_EVENT)",
                    error,
                ));
            }
        };
        if complete {
            let frame = self.submitted_frames.take_completed(slot_index)?;
            let result = frame.Close();
            drop(frame);
            result.map_err(|error| map_capture_error("Direct3D11CaptureFrame::Close", error))?;
        }
        Ok(complete)
    }

    fn release_frame(&mut self) -> Result<(), RecorderError> {
        if !self.acquired_outstanding {
            return Err(internal_error(
                "A capture frame release was requested without an outstanding frame.".to_string(),
            ));
        }
        self.acquired_outstanding = false;
        let Some(frame) = self.current.take() else {
            return Err(internal_error(
                "The outstanding capture frame was already released.".to_string(),
            ));
        };
        let Some(frame) = self.submitted_frames.release(frame) else {
            if let Some(state) = self.diagnostic_state.as_mut() {
                cursor_diagnostics::record("wgc.release", state.current, || {
                    serde_json::json!({
                        "copied_slot": state.copied_slot,
                        "source_frame_held_until_gpu_query": true,
                        "software_composition_count": 0, "success": true,
                    })
                });
                state.current = None;
            }
            return Ok(());
        };
        // No GPU copy was submitted, so this frame can return to the pool now.
        let result = frame.Close();
        drop(frame);
        if let Some(state) = self.diagnostic_state.as_mut() {
            cursor_diagnostics::record("wgc.release", state.current, || {
                serde_json::json!({
                    "copied_slot": state.copied_slot,
                    "source_frame_held_until_gpu_query": false,
                    "software_composition_count": 0, "success": result.is_ok(),
                })
            });
            state.current = None;
        }
        result.map_err(|error| map_capture_error("Direct3D11CaptureFrame::Close", error))
    }

    fn readback(&mut self, _slot_index: usize) -> ReadbackOutcome {
        // Opt-in pixel verification is a display-capture diagnostic and this session
        // never enables it. Refusing explicitly is what keeps that true: a session that
        // asked for verification would fail loudly instead of silently skipping it.
        ReadbackOutcome::Failed(internal_error(
            "Opt-in pixel verification is not implemented for window capture, so no readback was taken."
                .to_string(),
        ))
    }

    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    fn sample_counter(&mut self) -> Result<i64, RecorderError> {
        let mut value = 0i64;
        // SAFETY: the out-parameter is a valid initialized integer.
        unsafe { QueryPerformanceCounter(&mut value) }
            .map_err(|error| map_capture_error("QueryPerformanceCounter", error))?;
        Ok(value)
    }

    fn description(&self) -> &NativeDescription {
        &self.description
    }

    fn payload_owner(&self) -> SharedPayload {
        Arc::clone(&self.backing) as SharedPayload
    }

    fn finish(&mut self) -> Result<(), RecorderError> {
        // A failed frame Close must not skip the pool/session teardown. Drop is only a
        // safety net and cannot report its cleanup error to the recording owner.
        let mut frame_failure = if self.acquired_outstanding {
            self.release_frame().err()
        } else {
            None
        };
        // Runtime may discard pending destination slots during shutdown. Their source
        // frames still have to remain checked out until the GPU reads are complete.
        let deadline = Instant::now() + COPY_RETIRE_TIMEOUT;
        for slot_index in 0..self.submitted_frames.in_flight.len() {
            while self.submitted_frames.in_flight[slot_index].is_some() {
                match self.query_complete(slot_index) {
                    Ok(true) => break,
                    Ok(false) if Instant::now() < deadline => std::thread::yield_now(),
                    Ok(false) => {
                        let error = internal_error(format!(
                            "Timed out waiting for capture slot {slot_index} to finish its GPU copy."
                        ));
                        frame_failure = Some(match frame_failure {
                            Some(primary) => merge_release_failure(Some(primary), error),
                            None => error,
                        });
                        break;
                    }
                    Err(error) => {
                        frame_failure = Some(match frame_failure {
                            Some(primary) => merge_release_failure(Some(primary), error),
                            None => error,
                        });
                        break;
                    }
                }
            }
        }
        // On device failure or timeout, no reliable completion signal remains. Close
        // every remaining frame explicitly before the pool, and surface the error.
        for frame in self
            .submitted_frames
            .in_flight
            .iter_mut()
            .filter_map(Option::take)
        {
            let close = frame.Close();
            drop(frame);
            if let Err(error) = close {
                let error = map_capture_error("Direct3D11CaptureFrame::Close", error);
                frame_failure = Some(match frame_failure {
                    Some(primary) => merge_release_failure(Some(primary), error),
                    None => error,
                });
            }
        }
        let close_failure = self.capture.close_checked().err();
        match (frame_failure, close_failure) {
            (Some(primary), Some(cleanup)) => Err(merge_release_failure(Some(primary), cleanup)),
            (Some(primary), None) => Err(primary),
            (None, Some(cleanup)) => Err(cleanup),
            (None, None) => Ok(()),
        }
    }
}

impl WindowSource {
    /// Interprets one acquired frame: geometry, monotonic native time and the one-time
    /// frame-time-base measurement.
    ///
    /// A resize frame is closed before the pool is recreated on a later owner
    /// turn. Hidden/minimized frames never resize the recording canvas.
    fn observe(
        &mut self,
        frame: &Direct3D11CaptureFrame,
    ) -> Result<Option<FrameObservation>, RecorderError> {
        // SAFETY: the frame is alive and owned by this worker thread.
        let content = frame
            .ContentSize()
            .map_err(|error| map_capture_error("Direct3D11CaptureFrame::ContentSize", error))?;
        let current = self.identity.verify()?;
        let root = if self.identity == self.capture_identity {
            current
        } else {
            self.capture_identity.verify()?
        };
        if current.minimized
            || !current.visible
            || root.minimized
            || !root.visible
            || content.Width <= 0
            || content.Height <= 0
        {
            self.note_unpublishable();
            return Ok(None);
        }
        if content.Width != self.width as i32 || content.Height != self.height as i32 {
            self.pending_resize = Some(content);
            self.note_unpublishable();
            return Ok(None);
        }
        self.region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: self.width,
            bottom: self.height,
            back: 1,
        };
        if self.identity != self.capture_identity {
            let bounds = self.capture_identity.frame_bounds().ok_or_else(|| {
                source_lost_error("The capture root has no physical bounds.".into())
            })?;
            let selected = self.identity.frame_bounds().ok_or_else(|| {
                source_lost_error("The selected child has no physical bounds.".into())
            })?;
            self.region.left = selected
                .left
                .saturating_sub(bounds.left)
                .clamp(0, self.width as i32) as u32;
            self.region.top = selected
                .top
                .saturating_sub(bounds.top)
                .clamp(0, self.height as i32) as u32;
            self.region.right = selected
                .right
                .saturating_sub(bounds.left)
                .clamp(0, self.width as i32) as u32;
            self.region.bottom = selected
                .bottom
                .saturating_sub(bounds.top)
                .clamp(0, self.height as i32) as u32;
            if self.region.right <= self.region.left || self.region.bottom <= self.region.top {
                self.note_unpublishable();
                return Ok(None);
            }
        }
        let size = (
            self.region.right - self.region.left,
            self.region.bottom - self.region.top,
        );
        let direct = size.0 & !1 == self.output_width && size.1 & !1 == self.output_height;
        if !direct
            && self
                .fit
                .as_ref()
                .is_some_and(|fit| fit.input_size() != Some(size))
            && self.submitted_frames.in_flight.iter().any(Option::is_some)
        {
            // A child may resize without its root resizing. Retire previous draws
            // before replacing their scratch surface so GPU memory stays bounded.
            self.note_unpublishable();
            return Ok(None);
        }
        // SAFETY: same, and the returned span is a plain value.
        let relative = frame
            .SystemRelativeTime()
            .map_err(|error| {
                map_capture_error("Direct3D11CaptureFrame::SystemRelativeTime", error)
            })?
            .Duration;
        let ticks = frame_time_to_ticks(relative, self.ticks_per_second)?;
        let trace = self
            .diagnostic_state
            .as_ref()
            .and_then(|state| state.current);
        cursor_diagnostics::record("wgc.source_time", trace, || {
            let mut sampled_qpc = 0;
            // SAFETY: diagnostic-only sample of the same counter; failure does
            // not alter native source timing or admission.
            let sampled = unsafe { QueryPerformanceCounter(&mut sampled_qpc) }.is_ok();
            serde_json::json!({
                "system_relative_time_100ns": relative,
                "frame_qpc": ticks,
                "sampled_qpc": sampled.then_some(sampled_qpc),
                "qpc_frequency": self.ticks_per_second,
                "source_time_policy": "system_relative_time_to_qpc",
                "cursor_capture_enabled": self.diagnostic_state.as_ref().map(|state| state.cursor_capture_enabled),
                "software_composition_count": 0,
                "embedded_in_pixels": "unknown",
            })
        });
        // The frame time base is measured once per session, on its first published frame.
        if self.timestamp_base.get().is_none() {
            self.verify_timestamp_base(ticks)?;
        }
        Ok(Some(FrameObservation {
            last_present_time: ticks,
            last_mouse_update_time: 0,
            synthetic_cursor_time: false,
            synthetic_replay_time: false,
            // A window item reports no accumulated-update counter; reporting zero is the
            // honest answer, and it is why no fabricated "missed update" count appears in
            // this session's diagnostics.
            accumulated_frames: 0,
            // Windows.Graphics.Capture exposes no protected-content flag. If the OS
            // withholds a protected window's content, this session reports the resulting
            // poll timeouts and publishes no fabricated frame; it never claims to know
            // whether content was masked.
            protected_content_masked_out: false,
            trace,
        }))
    }

    /// Measures, once per session, that the frame time really is on the native counter.
    ///
    /// The capture API documents 100 ns units; this measures the first frame against a
    /// counter read taken immediately after it arrived, so a wrong unit or a different
    /// epoch is a bounded explicit failure instead of timestamps that silently sit
    /// somewhere else on the session timeline.
    fn verify_timestamp_base(&self, frame_ticks: i64) -> Result<(), RecorderError> {
        let mut sampled = 0i64;
        // SAFETY: the out-parameter is a valid initialized integer.
        unsafe { QueryPerformanceCounter(&mut sampled) }
            .map_err(|error| map_capture_error("QueryPerformanceCounter", error))?;
        let evidence = TimestampBaseEvidence {
            ticks_per_second: self.ticks_per_second,
            frame_units_per_second: FRAME_UNITS_PER_SECOND,
            first_frame_ticks: frame_ticks,
            sampled_ticks: sampled,
        };
        if evidence.ticks_per_second == 0 {
            return Err(internal_error(
                "QueryPerformanceFrequency reported zero, so capture frame times cannot be measured."
                    .to_string(),
            ));
        }
        let age = evidence.age_ticks();
        let max_age = duration_to_ticks(MAX_FRAME_AGE, evidence.ticks_per_second);
        let max_lead = duration_to_ticks(MAX_FRAME_LEAD, evidence.ticks_per_second);
        if age > max_age || age < -max_lead {
            return Err(internal_error(format!(
                "The first captured frame's time is {:.3} ms away from the session's native counter, outside the accepted -{:.0}..{:.0} ms window, so the capture frame time base cannot be trusted for this session.",
                evidence.age_milliseconds(),
                MAX_FRAME_LEAD.as_secs_f64() * 1000.0,
                MAX_FRAME_AGE.as_secs_f64() * 1000.0
            )));
        }
        // `set` fails only when it was already set, which means a previous frame already
        // measured the same session: the first measurement is the recorded evidence.
        let _ = self.timestamp_base.set(evidence);
        Ok(())
    }

    fn texture(&self, slot_index: usize) -> Result<&ID3D11Texture2D, RecorderError> {
        self.backing.texture(slot_index).ok_or_else(|| {
            internal_error(format!(
                "Slot {slot_index} is outside the session's {SLOT_COUNT} preallocated textures."
            ))
        })
    }

    pub(super) fn slot_mutex(&self, slot_index: usize) -> Result<IDXGIKeyedMutex, RecorderError> {
        self.texture(slot_index)?
            .cast()
            .map_err(|error| map_capture_error("IDXGIKeyedMutex(WGC slot)", error))
    }

    fn query(&self, slot_index: usize) -> Result<&ID3D11Query, RecorderError> {
        self.queries.get(slot_index).ok_or_else(|| {
            internal_error(format!(
                "Slot {slot_index} is outside the session's {SLOT_COUNT} preallocated queries."
            ))
        })
    }

    /// Counts one frame this session refused to publish.
    fn note_unpublishable(&self) {
        self.unpublishable_updates.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for WindowSource {
    fn drop(&mut self) {
        // Also covers worker unwinds; normal completion already drained these frames.
        let _ = self.finish();
    }
}

pub(super) struct CaptureGeometry {
    pub source: (u32, u32),
    pub output: (u32, u32),
}

/// Opens the Windows.Graphics.Capture session for one verified window.
///
/// Preparation is bounded and cancellable between stages; nothing is left running if it
/// fails, and no texture is created before the owned payload has been preflighted.
pub(super) fn open_shared(
    identity: &WindowIdentity,
    geometry: CaptureGeometry,
    timestamp_base: &Arc<OnceLock<TimestampBaseEvidence>>,
    policy: &Arc<OnceLock<CapturePolicyEvidence>>,
    unpublishable_updates: &Arc<AtomicU64>,
    preparation: &CapturePreparation<'_>,
    shared: ([HANDLE; SLOT_COUNT], u64),
) -> Result<WindowSource, RecorderError> {
    open_inner(
        identity,
        geometry,
        timestamp_base,
        policy,
        unpublishable_updates,
        preparation,
        Some(shared),
    )
}

fn open_inner(
    identity: &WindowIdentity,
    geometry: CaptureGeometry,
    timestamp_base: &Arc<OnceLock<TimestampBaseEvidence>>,
    policy: &Arc<OnceLock<CapturePolicyEvidence>>,
    unpublishable_updates: &Arc<AtomicU64>,
    preparation: &CapturePreparation<'_>,
    shared: Option<([HANDLE; SLOT_COUNT], u64)>,
) -> Result<WindowSource, RecorderError> {
    let CaptureGeometry {
        source: expected_geometry,
        output: output_geometry,
    } = geometry;
    let witness = crate::witness::Witness::new()?;
    let cancellation = preparation.cancellation;
    if cancellation.is_cancelled() {
        return Err(preparation_cancelled());
    }
    let apartment = ComApartment::initialize_mta()?;

    // The runtime capability check. This is the Windows build's own answer, not a
    // compile-time assumption, and this backend never raises the application's declared
    // minimum Windows version to make the answer yes.
    let supported = GraphicsCaptureSession::IsSupported()
        .map_err(|error| map_capture_error("GraphicsCaptureSession::IsSupported", error))?;
    if !supported {
        return Err(not_implemented_error(
            "Windows.Graphics.Capture reports that window capture sessions are not supported on this Windows build, and this backend has no fallback capture API."
                .to_string(),
        ));
    }

    let mut frequency = 0i64;
    // SAFETY: the out-parameter is a valid initialized integer.
    unsafe { QueryPerformanceFrequency(&mut frequency) }
        .map_err(|error| map_capture_error("QueryPerformanceFrequency", error))?;
    let Some(ticks_per_second) = std::num::NonZeroU64::new(u64::try_from(frequency).unwrap_or(0))
    else {
        return Err(internal_error(
            "QueryPerformanceFrequency reported zero, so capture frame times cannot be measured."
                .to_string(),
        ));
    };

    if cancellation.is_cancelled() {
        return Err(preparation_cancelled());
    }
    // SAFETY: CreateDXGIFactory1 has no preconditions and returns an owned COM object.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_capture_error("CreateDXGIFactory1", error))?;
    let (adapter, adapter_description) = select_adapter(&factory)?;
    if let Some((_, expected_luid)) = shared {
        let desc = unsafe { adapter.GetDesc1() }
            .map_err(|error| map_capture_error("IDXGIAdapter1::GetDesc1", error))?;
        let actual = (u64::from(desc.AdapterLuid.HighPart as u32) << 32)
            | u64::from(desc.AdapterLuid.LowPart);
        if actual != expected_luid {
            return Err(source_lost_error(format!(
                "WGC child adapter LUID {actual:016X} differs from parent {expected_luid:016X}."
            )));
        }
    }

    if cancellation.is_cancelled() {
        return Err(preparation_cancelled());
    }
    let (device, context) = create_device(&adapter)?;
    if cancellation.is_cancelled() {
        return Err(preparation_cancelled());
    }

    // WGC owns top-level surfaces. A child is cropped from this exact root,
    // whose identity is retained and verified independently for the entire session.
    let selected = identity.start_observation()?;
    if selected.frame_size != Some(expected_geometry) {
        return Err(source_lost_error(
            "The selected window resized during capture preparation; retry.".into(),
        ));
    }
    let capture_identity = identity.capture_root()?;
    let capture_start = capture_identity.start_observation()?;
    capture_identity.require_capturable(capture_start.cloaked)?;
    let capture_geometry = capture_start.frame_size.ok_or_else(|| {
        source_lost_error("The selected window's capture root has no usable bounds.".into())
    })?;
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(
            |error| {
                not_implemented_error(format!(
                    "The Windows.Graphics.Capture item factory is unavailable on this Windows build (HRESULT 0x{:08X}), and this backend has no fallback capture API.",
                    error.code().0
                ))
            },
        )?;
    // SAFETY: the interop factory is alive, the handle is the verified window handle, and
    // the returned item is an owned WinRT object.
    let item: GraphicsCaptureItem =
        unsafe { interop.CreateForWindow(HWND(capture_identity.handle() as *mut c_void)) }
            .map_err(|error| {
                map_capture_error("IGraphicsCaptureItemInterop::CreateForWindow", error)
            })?;
    // A window whose owning process changed between verification and item creation must
    // not be captured: the item would describe a different window than the caller chose.
    identity.verify()?;
    capture_identity.verify()?;
    if identity.capture_root()? != capture_identity {
        return Err(source_lost_error(
            "The selected child was reparented during capture preparation.".into(),
        ));
    }

    let size = item
        .Size()
        .map_err(|error| map_capture_error("GraphicsCaptureItem::Size", error))?;
    if size.Width != capture_geometry.0 as i32 || size.Height != capture_geometry.1 as i32 {
        return Err(source_lost_error(format!(
            "The selected window's capture item reports {}x{} while its frame bounds were read as {}x{}, so the window changed while capture was prepared and the session was not started.",
            size.Width, size.Height, capture_geometry.0, capture_geometry.1
        )));
    }
    let width = capture_geometry.0;
    let height = capture_geometry.1;
    crate::surface::plan_owned_payload(width, height, false)?;
    if output_geometry.0 != (expected_geometry.0 & !1)
        || output_geometry.1 != (expected_geometry.1 & !1)
        || output_geometry.0 == 0
        || output_geometry.1 == 0
    {
        return Err(internal_error(
            "The WGC output geometry must crop at most one right/bottom pixel to even dimensions."
                .into(),
        ));
    }

    // The WinRT device is the D3D11 device the capture frames will be composed into, so
    // the accepted hardware encoder can register the published textures directly.
    let dxgi_device: IDXGIDevice = device.cast().map_err(|error| {
        internal_error(format!(
            "The capture device does not expose IDXGIDevice: HRESULT 0x{:08X}.",
            error.code().0
        ))
    })?;
    // SAFETY: the DXGI device is alive for the call and the returned object is owned.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .map_err(|error| map_capture_error("CreateDirect3D11DeviceFromDXGIDevice", error))?;
    let winrt_device: IDirect3DDevice = inspectable.cast().map_err(|error| {
        internal_error(format!(
            "The WinRT device object is not an IDirect3DDevice: HRESULT 0x{:08X}.",
            error.code().0
        ))
    })?;

    let logical_size = windows::Graphics::SizeInt32 {
        Width: i32::try_from(width).unwrap_or(i32::MAX),
        Height: i32::try_from(height).unwrap_or(i32::MAX),
    };
    // Free-threaded: no dispatcher queue or message pump. Its arrival callback only
    // signals an event; all capture and D3D work stays on this worker thread.
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        WINDOW_FRAME_POOL_BUFFERS,
        logical_size,
    )
    .map_err(|error| map_capture_error("Direct3D11CaptureFramePool::CreateFreeThreaded", error))?;
    let session = match pool.CreateCaptureSession(&item) {
        Ok(session) => session,
        Err(error) => {
            let primary =
                map_capture_error("Direct3D11CaptureFramePool::CreateCaptureSession", error);
            return Err(match pool.Close() {
                Ok(()) => primary,
                Err(cleanup) => merge_release_failure(
                    Some(primary),
                    map_capture_error("Direct3D11CaptureFramePool::Close", cleanup),
                ),
            });
        }
    };
    let mut capture = CaptureRuntime::new(pool, session);
    // Frame arrivals are delivered through the pool's own event, not by polling
    // `TryGetNextFrame` in a tight loop. The handler runs on the pool's delivery thread
    // and only sets an auto-reset event, so the worker keeps its single-threaded,
    // bounded-wait structure and no callback ever touches capture state.
    capture
        .register_arrival()
        .map_err(|error| preparation_failure(&mut capture, error))?;
    // WGC is the sole cursor compositor for this source. This content preference
    // does not change OS capture permissions or the native capture border.
    capture
        .session
        .SetIsCursorCaptureEnabled(preparation.show_cursor)
        .map_err(|error| {
            preparation_failure(
                &mut capture,
                map_capture_error("GraphicsCaptureSession::SetIsCursorCaptureEnabled", error),
            )
        })?;
    let cursor_enabled = capture.session.IsCursorCaptureEnabled().map_err(|error| {
        preparation_failure(
            &mut capture,
            map_capture_error("GraphicsCaptureSession::IsCursorCaptureEnabled", error),
        )
    })?;
    if cursor_enabled != preparation.show_cursor {
        return Err(preparation_failure(
            &mut capture,
            internal_error("WGC did not acknowledge the requested cursor setting.".into()),
        ));
    }
    let _ = policy.set(CapturePolicyEvidence {
        cursor_capture_enabled: Some(cursor_enabled),
        border_required: capture.session.IsBorderRequired().ok(),
    });
    if cancellation.is_cancelled() {
        return Err(preparation_failure(&mut capture, preparation_cancelled()));
    }

    let texture_desc = D3D11_TEXTURE2D_DESC {
        Width: output_geometry.0,
        Height: output_geometry.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut sink = WindowTextureSink {
        device: &device,
        surface_desc: texture_desc,
        query_desc: D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        },
        textures: Vec::with_capacity(SLOT_COUNT),
        queries: Vec::with_capacity(SLOT_COUNT),
        shared_handles: shared.map(|(handles, _)| handles),
    };
    // Preflight, then allocate: a rejected budget creates nothing. Window capture takes
    // no readback staging texture, so the plan is the four owned surfaces alone, and the
    // control thread's preflight against the frame bounds is repeated here against the
    // capture item's own size before anything is allocated.
    prepare_owned_payload(output_geometry.0, output_geometry.1, false, &mut sink)
        .map_err(|error| preparation_failure(&mut capture, error))?;
    let WindowTextureSink {
        textures, queries, ..
    } = sink;
    // Shader/target setup stays out of frame delivery. An adapter unable to fit
    // may still use native-size capture; a later resize reports an explicit error.
    let (fit, fit_error) =
        match crate::compositor::fit::TextureFit::new(&device, &textures, output_geometry) {
            Ok(fit) => (Some(fit), None),
            Err(error) => (None, Some(error)),
        };
    // Resource/shader preparation cannot age the first frame of a static source.
    // Capture begins only after all startup allocations and identity checks.
    if cancellation.is_cancelled() {
        return Err(preparation_failure(&mut capture, preparation_cancelled()));
    }
    identity
        .verify()
        .map_err(|error| preparation_failure(&mut capture, error))?;
    capture_identity
        .verify()
        .map_err(|error| preparation_failure(&mut capture, error))?;
    capture.session.StartCapture().map_err(|error| {
        preparation_failure(
            &mut capture,
            map_capture_error("GraphicsCaptureSession::StartCapture", error),
        )
    })?;

    let description = NativeDescription {
        api: CAPTURE_API.to_string(),
        adapter_description,
        output_gdi_name: identity.id().0,
        width: output_geometry.0,
        height: output_geometry.1,
        pixel_format: PIXEL_FORMAT.to_string(),
        // Window content carries no DXGI rotation. The initial canvas stays fixed
        // across source resizes; resized content is fitted on the GPU.
        rotation_identity: true,
        desktop_image_in_system_memory: false,
        context_multithread_protected: true,
        staging_bytes: 0,
    };

    let backing = GpuSurfaceSet::new(
        device,
        textures,
        output_geometry.0,
        output_geometry.1,
        preparation.pool_id,
        Arc::clone(preparation.lifetime),
    );

    Ok(WindowSource {
        witness,
        diagnostic_state: WindowDiagnosticState::new(cursor_enabled),
        context,
        queries,
        backing,
        description,
        identity: *identity,
        capture_identity,
        _item: item,
        capture,
        winrt_device,
        current: None,
        acquired_outstanding: false,
        submitted_frames: SubmittedFrameHolds::new(),
        pending_resize: None,
        region: D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        },
        fit,
        fit_error,
        width,
        height,
        output_width: output_geometry.0,
        output_height: output_geometry.1,
        ticks_per_second: ticks_per_second.get(),
        timestamp_base: Arc::clone(timestamp_base),
        unpublishable_updates: Arc::clone(unpublishable_updates),
        _apartment: apartment,
    })
}

/// The device-backed texture sink used by production.
///
/// It implements the shared [`TextureSink`] seam, so the preflight-then-allocate rule is
/// literally the display path's code; only the descriptors differ, because a window
/// session allocates no readback staging texture.
struct WindowTextureSink<'a> {
    device: &'a ID3D11Device,
    surface_desc: D3D11_TEXTURE2D_DESC,
    query_desc: D3D11_QUERY_DESC,
    textures: Vec<ID3D11Texture2D>,
    queries: Vec<ID3D11Query>,
    shared_handles: Option<[HANDLE; SLOT_COUNT]>,
}

impl TextureSink for WindowTextureSink<'_> {
    fn create(&mut self, kind: TextureKind) -> Result<(), RecorderError> {
        if kind != TextureKind::Surface {
            return Err(internal_error(
                "Window capture allocates no staging texture, so a staging allocation request is a defect."
                    .to_string(),
            ));
        }
        let texture = if let Some(handles) = self.shared_handles {
            let device1: ID3D11Device1 = self
                .device
                .cast()
                .map_err(|error| map_capture_error("ID3D11Device1", error))?;
            let handle = handles[self.textures.len()];
            // SAFETY: the parent duplicated this NT shared texture handle into the
            // child and both devices were checked against the same adapter LUID.
            let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(handle) }
                .map_err(|error| map_capture_error("OpenSharedResource1(WGC slot)", error))?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            if desc.Width != self.surface_desc.Width
                || desc.Height != self.surface_desc.Height
                || desc.Format != self.surface_desc.Format
            {
                return Err(internal_error(
                    "Shared WGC slot geometry/format differs from the session.".into(),
                ));
            }
            texture
        } else {
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: initialized descriptor and owned out-parameter.
            unsafe {
                self.device
                    .CreateTexture2D(&self.surface_desc, None, Some(&mut texture))
            }
            .map_err(|error| map_capture_error("ID3D11Device::CreateTexture2D", error))?;
            texture.ok_or_else(|| {
                internal_error("CreateTexture2D reported success without a texture.".into())
            })?
        };
        // Each surface owns a per-slot event query, so a later poll cannot observe
        // another slot's completion.
        let mut query: Option<ID3D11Query> = None;
        // SAFETY: the descriptor is fully initialized and the API only writes into the
        // out-parameter.
        unsafe { self.device.CreateQuery(&self.query_desc, Some(&mut query)) }
            .map_err(|error| map_capture_error("ID3D11Device::CreateQuery", error))?;
        let query = query.ok_or_else(|| {
            internal_error("CreateQuery reported success without a query.".to_string())
        })?;
        self.textures.push(texture);
        self.queries.push(query);
        Ok(())
    }
}

/// Creates the parent-owned device used by the isolated WGC slot pool.
pub(super) fn parent_device() -> Result<(ID3D11Device, u64, String), RecorderError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_capture_error("CreateDXGIFactory1", error))?;
    let (adapter, description) = select_adapter(&factory)?;
    let desc = unsafe { adapter.GetDesc1() }
        .map_err(|error| map_capture_error("IDXGIAdapter1::GetDesc1", error))?;
    let luid =
        (u64::from(desc.AdapterLuid.HighPart as u32) << 32) | u64::from(desc.AdapterLuid.LowPart);
    let (device, _) = create_device(&adapter)?;
    Ok((device, luid, description))
}

/// Selects the Direct3D 11 adapter this session captures and encodes on.
///
/// The accepted hardware encoder is device-bound, so an NVIDIA adapter is preferred when
/// one exists: the captured textures then live on the device the encoder can register
/// them on, with no cross-adapter copy. When there is no NVIDIA adapter the first
/// non-software adapter is used and reported — a documented preference order, not a
/// silent switch of capture path. A machine with no hardware adapter is an explicit
/// error rather than a software device.
fn select_adapter(factory: &IDXGIFactory1) -> Result<(IDXGIAdapter1, String), RecorderError> {
    let mut first_hardware: Option<(IDXGIAdapter1, String)> = None;
    let mut index = 0u32;
    while (index as usize) < MAX_SCANNED_ADAPTERS {
        // SAFETY: adapter indices are probed until the documented not-found failure.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => {
                return Err(map_capture_error("IDXGIFactory1::EnumAdapters1", error));
            }
        };
        // SAFETY: the adapter is alive for the call.
        let description = match unsafe { adapter.GetDesc1() } {
            Ok(description) => description,
            Err(error) => return Err(map_capture_error("IDXGIAdapter1::GetDesc1", error)),
        };
        let text = wide_to_string(&description.Description);
        let software = description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
        if software {
            index += 1;
            continue;
        }
        if description.VendorId == NVIDIA_VENDOR_ID {
            return Ok((adapter, text));
        }
        if first_hardware.is_none() {
            first_hardware = Some((adapter, text));
        }
        index += 1;
    }
    first_hardware.ok_or_else(|| {
        not_implemented_error(format!(
            "No hardware Direct3D 11 adapter was reported within the bounded scan of {MAX_SCANNED_ADAPTERS} adapters, and window capture deliberately creates no software device."
        ))
    })
}

/// Creates the session's Direct3D 11 device and immediate context.
///
/// The multithread protection is enabled and verified before the device can be exposed to
/// the encoder, exactly as the display capture path does (ADR 0006 R2 addendum).
fn create_device(
    adapter: &IDXGIAdapter1,
) -> Result<(ID3D11Device, ID3D11DeviceContext), RecorderError> {
    let mut device: Option<ID3D11Device> = None;
    let mut feature_level = D3D_FEATURE_LEVEL(0);
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: the adapter is alive for the call, an explicit adapter requires
    // D3D_DRIVER_TYPE_UNKNOWN, and the out-parameters are valid optionals the API fills on
    // success. No software device is requested.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
    }
    .map_err(|error| map_capture_error("D3D11CreateDevice", error))?;
    let device = device.ok_or_else(|| {
        internal_error("D3D11CreateDevice reported success without a device.".to_string())
    })?;
    let context = context.ok_or_else(|| {
        internal_error(
            "D3D11CreateDevice reported success without an immediate context.".to_string(),
        )
    })?;
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
            "The selected adapter reports D3D feature level 0x{:X}, and window capture requires 11_0 or higher.",
            feature_level.0
        )));
    }
    Ok((device, context))
}

/// Converts one capture frame time to the session's native counter.
///
/// `SystemRelativeTime` is documented in 100 ns units and measured to be on the
/// `QueryPerformanceCounter` counter, so the conversion is an explicit scale by the
/// counter frequency: on the common 10 MHz counter it is the identity, and the result is
/// measured against the counter on the first frame of every session.
fn frame_time_to_ticks(relative: i64, ticks_per_second: u64) -> Result<i64, RecorderError> {
    if relative <= 0 {
        return Err(internal_error(format!(
            "The captured frame reported a non-positive system-relative time ({relative}), which cannot be placed on the session timeline."
        )));
    }
    let ticks = u128::from(relative as u64)
        .checked_mul(u128::from(ticks_per_second))
        .ok_or_else(|| {
            internal_error(
                "The captured frame's time overflowed while it was converted to the session's counter."
                    .to_string(),
            )
        })?
        / u128::from(FRAME_UNITS_PER_SECOND);
    i64::try_from(ticks).map_err(|_| {
        internal_error(
            "The captured frame's time does not fit the session's counter range.".to_string(),
        )
    })
}

fn duration_to_ticks(duration: Duration, ticks_per_second: u64) -> i64 {
    let nanos = duration.as_nanos();
    let ticks = nanos.saturating_mul(u128::from(ticks_per_second)) / 1_000_000_000u128;
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

fn wide_to_string(slice: &[u16]) -> String {
    let length = slice
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..length])
}

/// Maps a native failure onto the accepted error taxonomy, preserving the HRESULT.
///
/// `E_ACCESSDENIED` is the documented answer when the current desktop session may not
/// capture the window (for example a window owned by an elevated process). Device loss
/// and a closed capture object are lost sources; everything else is an internal failure,
/// so a defect can never be reported as an unsupported feature.
fn map_capture_error(api: &str, error: windows::core::Error) -> RecorderError {
    let code = error.code();
    let hresult = code.0;
    if code == E_ACCESSDENIED {
        return permission_denied_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the current desktop session denied capture of the selected window, which is what an elevated or protected window reports."
        ));
    }
    if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        return source_lost_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the capture device was removed or reset, so this session cannot continue."
        ));
    }
    // RO_E_CLOSED: the capture object was closed, which is what an ended capture session
    // reports.
    if code.0 == 0x8000_0013u32 as i32 {
        return source_lost_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the capture session was closed."
        ));
    }
    internal_error(format!("{api} failed with HRESULT 0x{hresult:08X}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn submitted_frame_stays_checked_out_until_matching_query_completes() {
        struct Frame(Rc<Cell<u32>>);
        impl Drop for Frame {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let closed = Rc::new(Cell::new(0));
        let mut holds = SubmittedFrameHolds::new();
        holds.mark_submitted(1).unwrap();
        assert!(holds.release(Frame(Rc::clone(&closed))).is_none());
        assert_eq!(closed.get(), 0);
        assert!(holds.mark_submitted(1).is_err());
        assert!(holds.take_completed(0).is_err());
        assert_eq!(closed.get(), 0);

        drop(holds.take_completed(1).unwrap());
        assert_eq!(closed.get(), 1);
        assert!(holds.take_completed(1).is_err());

        drop(holds.release(Frame(Rc::clone(&closed))));
        assert_eq!(closed.get(), 2);
    }

    #[test]
    fn frame_times_are_scaled_from_hundred_nanosecond_units() {
        // A 10 MHz counter is the common case and makes the conversion the identity.
        assert_eq!(
            frame_time_to_ticks(1_000_000, 10_000_000).unwrap(),
            1_000_000
        );
        // A 3.9 MHz counter scales down without losing the frame's order.
        assert_eq!(
            frame_time_to_ticks(10_000_000, 3_900_000).unwrap(),
            3_900_000
        );
        assert_eq!(frame_time_to_ticks(1, 10_000_000).unwrap(), 1);
        // Non-positive and overflowing inputs are explicit failures, never zero.
        assert!(frame_time_to_ticks(0, 10_000_000).is_err());
        assert!(frame_time_to_ticks(-5, 10_000_000).is_err());
        assert!(frame_time_to_ticks(i64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn timestamp_base_evidence_reports_a_signed_age() {
        let evidence = TimestampBaseEvidence {
            ticks_per_second: 10_000_000,
            frame_units_per_second: FRAME_UNITS_PER_SECOND,
            first_frame_ticks: 1_000_000,
            sampled_ticks: 1_030_000,
        };
        assert_eq!(evidence.age_ticks(), 30_000);
        assert!((evidence.age_milliseconds() - 3.0).abs() < f64::EPSILON);

        // A presentation time slightly ahead of the retrieval is a negative age, which
        // the accepted window allows within its lead bound.
        let ahead = TimestampBaseEvidence {
            sampled_ticks: 970_000,
            ..evidence
        };
        assert_eq!(ahead.age_ticks(), -30_000);

        // A zero frequency can never divide.
        let zero = TimestampBaseEvidence {
            ticks_per_second: 0,
            ..evidence
        };
        assert_eq!(zero.age_milliseconds(), 0.0);

        assert_eq!(duration_to_ticks(MAX_FRAME_AGE, 10_000_000), 10_000_000);
        assert_eq!(duration_to_ticks(MAX_FRAME_LEAD, 10_000_000), 500_000);
    }
}
