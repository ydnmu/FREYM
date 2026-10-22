#![allow(unsafe_code)] // Isolated DXGI shared texture and swapchain boundary.
//! Native SDR display presenter. The capture thread copies into one shared
//! GPU texture without waiting; another D3D11 device owns presentation.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
#[cfg(test)]
use windows::Win32::Graphics::Direct3D11::D3D11_BOX;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Device1,
    ID3D11DeviceContext, ID3D11Texture2D,
};
#[cfg(test)]
use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR, DCompositionCreateDevice, IDCompositionDevice,
    IDCompositionScaleTransform, IDCompositionSurface, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT_DO_NOT_WAIT,
    DXGI_SCALING_STRETCH, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIKeyedMutex, IDXGIResource1, IDXGISwapChain1,
};
#[cfg(test)]
use windows::Win32::Graphics::Dxgi::{IDXGISurface, IDXGISurface2};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GA_ROOT, GetAncestor, IsIconic, IsWindowVisible,
    SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, WS_CHILD,
    WS_CLIPSIBLINGS, WS_DISABLED, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
};
#[cfg(test)]
use windows::core::HRESULT;
use windows::core::w;
use windows::core::{BOOL, Interface, PCWSTR};

use crate::cursor_preview_diagnostics::{self as diagnostics, PreviewFrameTrace};
use crate::preview::PreviewSource;
use crate::surface::MAX_PAYLOAD_BYTES;

mod gpu_timing;
mod timing;
pub use timing::PreviewStageTiming;

/// GPU presentation has its own limit; the CPU/PNG preview's 10 FPS limit is
/// unrelated. Recording FPS and native image dimensions are unaffected.
pub const DEFAULT_GPU_PREVIEW_REFRESH_HZ: u32 = 30;
pub const MAX_GPU_PREVIEW_REFRESH_HZ: u32 = 60;

// Diagnostic identities do not expose native handles or change resource ownership.
static NEXT_DIAGNOSTIC_ID: AtomicU64 = AtomicU64::new(1);
const PENDING_RETRY_INTERVAL: Duration = Duration::from_millis(1);

/// Preview-only accounting, deliberately separate from recording frame drops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuPreviewStats {
    pub frames_considered: u64,
    pub rate_limited: u64,
    pub suspended: u64,
    pub producer_busy: u64,
    pub copied: u64,
    pub replaced_pending: u64,
    pub presented: u64,
    pub presenter_busy: u64,
    /// CPU time spent issuing CopyResource + Flush, not GPU execution latency.
    pub copy_submit_nanos: u64,
    pub copy_submit_cpu: PreviewStageTiming,
    pub mailbox_wait_cpu: PreviewStageTiming,
    pub presenter_submit_cpu: PreviewStageTiming,
    /// Timestamp/disjoint query span around presenter CopyResource/Flush.
    /// Includes command submission/resource waits; ends before DXGI Present.
    pub presenter_gpu_span: PreviewStageTiming,
    pub gpu_timing_unavailable: u64,
    pub gpu_timing_invalid: u64,
    pub gpu_timing_busy: u64,
    pub gpu_timing_unresolved: u64,
}

#[derive(Default)]
struct Counters {
    frames_considered: AtomicU64,
    rate_limited: AtomicU64,
    suspended: AtomicU64,
    producer_busy: AtomicU64,
    copied: AtomicU64,
    replaced_pending: AtomicU64,
    presented: AtomicU64,
    presenter_busy: AtomicU64,
    copy_submit_nanos: AtomicU64,
    copy_submit_cpu: timing::Timing,
    mailbox_wait_cpu: timing::Timing,
    presenter_submit_cpu: timing::Timing,
    presenter_gpu_span: timing::Timing,
    gpu_timing_unavailable: AtomicU64,
    gpu_timing_invalid: AtomicU64,
    gpu_timing_busy: AtomicU64,
    gpu_timing_unresolved: AtomicU64,
}

fn refresh_interval_nanos(hz: u32) -> Result<u64, String> {
    if !(1..=MAX_GPU_PREVIEW_REFRESH_HZ).contains(&hz) {
        return Err(format!(
            "GPU preview refresh rate must be between 1 and {MAX_GPU_PREVIEW_REFRESH_HZ} FPS."
        ));
    }
    // Round up: truncating the interval could exceed the requested ceiling.
    Ok(1_000_000_000_u64.div_ceil(u64::from(hz)))
}

/// Read-only Win32 queries are safe across threads. IsWindowVisible includes
/// the child's ancestors; a minimized root still has WS_VISIBLE, hence the
/// separate IsIconic check. HWND ownership remains with the UI thread.
fn window_drawable(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        IsWindowVisible(hwnd).as_bool() && !root.0.is_null() && !IsIconic(root).as_bool()
    }
}

/// These three HWND operations must be called on the owning UI thread.
pub fn create_child_window(parent: isize) -> Result<isize, String> {
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_CLIPSIBLINGS | WS_DISABLED,
            0,
            0,
            1,
            1,
            Some(HWND(parent as *mut c_void)),
            None,
            None,
            None,
        )
    }
    .map_err(|error| format!("GPU preview child window failed: {error}"))?;
    Ok(hwnd.0 as isize)
}

pub fn place_child_window(
    hwnd: isize,
    rect: (i32, i32, i32, i32),
    visible: bool,
) -> Result<(), String> {
    let (x, y, width, height) = rect;
    unsafe {
        SetWindowPos(
            HWND(hwnd as *mut c_void),
            None,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE
                | SWP_NOZORDER
                | if visible {
                    SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                },
        )
    }
    .map_err(|error| format!("GPU preview window placement failed: {error}"))
}

pub fn destroy_child_window(hwnd: isize) -> Result<(), String> {
    unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) }.map_err(|error| error.to_string())
}

pub(crate) struct SharedImage {
    diagnostic_id: u64,
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    adapter: IDXGIAdapter,
    handle: usize,
    width: u32,
    height: u32,
}

impl Drop for SharedImage {
    fn drop(&mut self) {
        // SAFETY: CreateSharedHandle returned this owned NT handle.
        let _ = unsafe { CloseHandle(HANDLE(self.handle as *mut c_void)) };
    }
}

#[derive(Default)]
struct State {
    image: Option<Arc<SharedImage>>,
    sequence: u64,
    submitted_at_ns: u64,
    published_ns: u64,
    frame: Option<PreviewFrameTrace>,
}

#[derive(Clone, Copy)]
struct Publication {
    sequence: u64,
    submitted_at_ns: u64,
    published_ns: u64,
    frame: Option<PreviewFrameTrace>,
}

impl State {
    fn publication(&self) -> Publication {
        Publication {
            sequence: self.sequence,
            submitted_at_ns: self.submitted_at_ns,
            published_ns: self.published_ns,
            frame: self.frame,
        }
    }
}

/// One producer-owned shared texture, with at most one pending or in-flight
/// picture. The producer may overwrite a pending picture before the presenter
/// takes it; an in-flight picture makes the producer drop the new preview.
pub struct GpuPreviewSink {
    diagnostic_id: u64,
    generation: u64,
    active: AtomicBool,
    visible: AtomicBool,
    hwnd: AtomicIsize,
    refresh_interval_nanos: AtomicU64,
    origin: Instant,
    /// Zero means no attempt yet; otherwise elapsed nanoseconds plus one.
    last_attempt_nanos: AtomicU64,
    timing_enabled: AtomicBool,
    counters: Counters,
    state: Mutex<State>,
    failure: std::sync::OnceLock<String>,
    ready: Condvar,
}

impl GpuPreviewSink {
    pub fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            diagnostic_id: NEXT_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed),
            generation,
            active: AtomicBool::new(true),
            visible: AtomicBool::new(true),
            hwnd: AtomicIsize::new(0),
            refresh_interval_nanos: AtomicU64::new(
                refresh_interval_nanos(DEFAULT_GPU_PREVIEW_REFRESH_HZ)
                    .expect("default GPU preview refresh rate is valid"),
            ),
            origin: Instant::now(),
            last_attempt_nanos: AtomicU64::new(0),
            timing_enabled: AtomicBool::new(false),
            counters: Counters::default(),
            state: Mutex::new(State::default()),
            failure: std::sync::OnceLock::new(),
            ready: Condvar::new(),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Geometry only; the UI never obtains pixel data or waits for the GPU.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        let state = self.state.try_lock().ok()?;
        state
            .image
            .as_ref()
            .map(|image| (image.width, image.height))
    }

    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Changes only GPU preview cadence. No settings schema or recording policy
    /// changes are needed; callers may choose any ceiling from 1 through 60 FPS.
    pub fn set_refresh_rate(&self, hz: u32) -> Result<(), String> {
        self.refresh_interval_nanos
            .store(refresh_interval_nanos(hz)?, Ordering::Release);
        Ok(())
    }

    /// UI intent complements native hidden/minimized detection. Closing or
    /// hiding a preview suspends copies without closing the recording session.
    pub fn set_visible(&self, visible: bool) {
        if self.visible.swap(visible, Ordering::AcqRel) != visible {
            diagnostics::record("preview_visibility", None, || {
                serde_json::json!({
                    "sink_id": self.diagnostic_id, "generation": self.generation, "visible": visible,
                })
            });
        }
    }

    /// Diagnostics only. Storage is fixed and no extra lock/readback/GPU wait is
    /// introduced. Call before a measurement window; cumulative snapshots are
    /// separate atomic observations, not one global transaction.
    pub fn set_timing_enabled(&self, enabled: bool) {
        self.timing_enabled.store(enabled, Ordering::Release);
    }

    pub fn stats(&self) -> GpuPreviewStats {
        let c = &self.counters;
        GpuPreviewStats {
            frames_considered: c.frames_considered.load(Ordering::Relaxed),
            rate_limited: c.rate_limited.load(Ordering::Relaxed),
            suspended: c.suspended.load(Ordering::Relaxed),
            producer_busy: c.producer_busy.load(Ordering::Relaxed),
            copied: c.copied.load(Ordering::Relaxed),
            replaced_pending: c.replaced_pending.load(Ordering::Relaxed),
            presented: c.presented.load(Ordering::Relaxed),
            presenter_busy: c.presenter_busy.load(Ordering::Relaxed),
            copy_submit_nanos: c.copy_submit_nanos.load(Ordering::Relaxed),
            copy_submit_cpu: c.copy_submit_cpu.snapshot(),
            mailbox_wait_cpu: c.mailbox_wait_cpu.snapshot(),
            presenter_submit_cpu: c.presenter_submit_cpu.snapshot(),
            presenter_gpu_span: c.presenter_gpu_span.snapshot(),
            gpu_timing_unavailable: c.gpu_timing_unavailable.load(Ordering::Relaxed),
            gpu_timing_invalid: c.gpu_timing_invalid.load(Ordering::Relaxed),
            gpu_timing_busy: c.gpu_timing_busy.load(Ordering::Relaxed),
            gpu_timing_unresolved: c.gpu_timing_unresolved.load(Ordering::Relaxed),
        }
    }

    fn drawable(&self) -> bool {
        self.visible.load(Ordering::Acquire) && window_drawable(self.hwnd.load(Ordering::Acquire))
    }

    /// Capture calls this before obtaining a preview source or touching D3D.
    /// Reserve the interval even if the subsequent nonblocking copy is busy,
    /// so a stalled presenter cannot cause per-capture-frame mutex attempts.
    pub(crate) fn admit_frame(&self) -> bool {
        self.admit_at(self.origin.elapsed(), self.drawable())
    }

    fn admit_at(&self, elapsed: Duration, drawable: bool) -> bool {
        if !self.active() {
            return false;
        }
        self.counters
            .frames_considered
            .fetch_add(1, Ordering::Relaxed);
        if !drawable {
            self.counters.suspended.fetch_add(1, Ordering::Relaxed);
            self.trace_admission("suspended");
            return false;
        }
        let now = elapsed.as_nanos().min(u128::from(u64::MAX - 1)) as u64;
        let last = self.last_attempt_nanos.load(Ordering::Acquire);
        if last != 0
            && now.saturating_sub(last - 1) < self.refresh_interval_nanos.load(Ordering::Acquire)
        {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            self.trace_admission("rate_limited");
            return false;
        }
        if self
            .last_attempt_nanos
            .compare_exchange(last, now + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            self.trace_admission("contended");
            return false;
        }
        self.trace_admission("admitted");
        true
    }

    fn trace_admission(&self, outcome: &'static str) {
        diagnostics::record("preview_admission", None, || {
            serde_json::json!({
                "sink_id": self.diagnostic_id, "generation": self.generation,
                "outcome": outcome,
                "refresh_interval_ns": self.refresh_interval_nanos.load(Ordering::Relaxed),
            })
        });
    }

    pub fn error(&self) -> Option<String> {
        if let Some(error) = self.failure.get() {
            return Some(error.clone());
        }
        None
    }

    pub fn close(&self) {
        // Change the wait predicate under the same mutex as wait_next. Hidden
        // previews may have no further frames to wake a lost close notification.
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.active.store(false, Ordering::Release);
        drop(state);
        self.ready.notify_all();
    }

    pub(crate) fn fail(&self, error: String) {
        diagnostics::record("preview_failure", None, || {
            serde_json::json!({
                "sink_id": self.diagnostic_id, "generation": self.generation, "error": error,
            })
        });
        let _ = self.failure.set(error);
        self.active.store(false, Ordering::Release);
        self.ready.notify_all();
    }

    /// Install resources allocated during session preparation, with no native
    /// allocation or waiting in capture's frame loop.
    pub(crate) fn prepare_image(&self, image: &Arc<SharedImage>) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            self.counters.producer_busy.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if state.image.is_none() {
            state.image = Some(Arc::clone(image));
            diagnostics::record("preview_texture", None, || {
                serde_json::json!({
                    "sink_id": self.diagnostic_id, "generation": self.generation,
                    "texture_id": image.diagnostic_id, "width": image.width, "height": image.height,
                })
            });
        }
        true
    }

    /// Called only on the capture thread with that thread's immediate context.
    /// Call only after `admit_frame` accepts the completed source. `AcquireSync(0)`
    /// never waits; no capture/NVENC slot is retained.
    pub(crate) fn submit(
        &self,
        source: &PreviewSource,
        _device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        capture_payload_bytes: u64,
    ) -> Result<(), String> {
        self.submit_with_frame(source, _device, context, capture_payload_bytes, None)
    }

    pub(crate) fn submit_with_frame(
        &self,
        source: &PreviewSource,
        _device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        _capture_payload_bytes: u64,
        frame: Option<PreviewFrameTrace>,
    ) -> Result<(), String> {
        if !self.active() {
            return Ok(());
        }
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                self.counters.producer_busy.fetch_add(1, Ordering::Relaxed);
                diagnostics::record("preview_producer_busy", frame.map(|f| f.capture), || {
                    serde_json::json!({
                        "sink_id": self.diagnostic_id, "generation": self.generation, "reason": "mailbox",
                    })
                });
                return Ok(());
            }
            Err(TryLockError::Poisoned(_)) => return Err("GPU preview state was poisoned.".into()),
        };
        let image = if let Some(image) = state.image.as_ref() {
            Arc::clone(image)
        } else {
            return Err("GPU preview storage was not prepared before frame submission.".into());
        };
        if (image.width, image.height) != (source.width, source.height) {
            return Err("GPU preview source dimensions changed within one generation.".into());
        }
        // Key 0 is free after presentation; key 1 is a pending image that the
        // presenter has not acquired. Taking either key with a zero timeout
        // replaces the one latest picture. A presenter-owned image is skipped.
        let replaced_pending = if try_acquire(&image.mutex, 0)? {
            false
        } else if try_acquire(&image.mutex, 1)? {
            true
        } else {
            self.counters.producer_busy.fetch_add(1, Ordering::Relaxed);
            diagnostics::record("preview_producer_busy", frame.map(|f| f.capture), || {
                serde_json::json!({
                    "sink_id": self.diagnostic_id, "generation": self.generation,
                    "texture_id": image.diagnostic_id, "reason": "keyed_mutex",
                })
            });
            return Ok(());
        };
        diagnostics::record("preview_producer_acquire", frame.map(|f| f.capture), || {
            serde_json::json!({
                "sink_id": self.diagnostic_id, "generation": self.generation,
                "texture_id": image.diagnostic_id, "key": u64::from(replaced_pending),
                "replaced_sequence": replaced_pending.then_some(state.sequence),
            })
        });
        let copy_started = Instant::now();
        let trace_started_ns = diagnostics::enabled().then(diagnostics::now_ns);
        // SAFETY: source and destination are same-format textures on this
        // capture device; the keyed mutex excludes the second device.
        unsafe { context.CopyResource(&image.texture, &source.texture) };
        let trace_copy_end_ns = trace_started_ns.map(|_| diagnostics::now_ns());
        // Submit the copy before handing the shared resource to the other device.
        unsafe { context.Flush() };
        let trace_flush_end_ns = trace_started_ns.map(|_| diagnostics::now_ns());
        let copy_ns = copy_started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.counters
            .copy_submit_nanos
            .fetch_add(copy_ns, Ordering::Relaxed);
        let timed = self.timing_enabled.load(Ordering::Acquire) || diagnostics::enabled();
        if timed {
            self.counters.copy_submit_cpu.record(copy_ns);
        }
        // SAFETY: this thread owns the mutex acquired above.
        unsafe { image.mutex.ReleaseSync(1) }
            .map_err(|error| format!("GPU preview producer release failed: {error}"))?;
        state.sequence = state.sequence.wrapping_add(1).max(1);
        state.submitted_at_ns = if timed {
            self.origin.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
        } else {
            0
        };
        state.published_ns = trace_started_ns.map(|_| diagnostics::now_ns()).unwrap_or(0);
        state.frame = frame;
        self.counters.copied.fetch_add(1, Ordering::Relaxed);
        if replaced_pending {
            self.counters
                .replaced_pending
                .fetch_add(1, Ordering::Relaxed);
        }
        diagnostics::record("preview_publish", frame.map(|f| f.capture), || {
            serde_json::json!({
                "sink_id": self.diagnostic_id, "generation": self.generation,
                "texture_id": image.diagnostic_id, "preview_sequence": state.sequence,
                "released_key": 1, "replaced_pending": replaced_pending,
                "copy_submit_ns": trace_copy_end_ns.zip(trace_started_ns).map(|(end,start)| end.saturating_sub(start)),
                "flush_ns": trace_flush_end_ns.zip(trace_copy_end_ns).map(|(end,start)| end.saturating_sub(start)),
                "copy_flush_cpu_ns": copy_ns, "published_ns": state.published_ns,
                "native_ticks": frame.map(|f| f.native_ticks), "media_ns": frame.and_then(|f| f.media_ns),
                "completed_ns": frame.map(|f| f.completed_ns),
            })
        });
        self.ready.notify_one();
        Ok(())
    }

    fn wait_next(&self, seen: u64) -> Option<(Arc<SharedImage>, Publication)> {
        let mut state = self.state.lock().ok()?;
        while self.active() && (state.sequence == seen || state.image.is_none()) {
            // Failure publication uses no mailbox lock. Recheck closes its
            // notify-before-wait race without blocking the capture producer.
            state = self
                .ready
                .wait_timeout(state, Duration::from_millis(100))
                .ok()?
                .0;
        }
        if !self.active() {
            return None;
        }
        Some((Arc::clone(state.image.as_ref()?), state.publication()))
    }

    /// Claim pixels and their current metadata in the same critical section.
    /// The producer always uses try_lock, and acquire must use a zero timeout.
    /// No GPU copy, flush, resource allocation or wait occurs under this lock.
    fn claim_current_with(
        &self,
        seen: &mut u64,
        acquire: impl FnOnce() -> Result<bool, String>,
    ) -> Result<Option<Publication>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "GPU preview state was poisoned.")?;
        if !self.active() || state.sequence == *seen {
            return Ok(None);
        }
        if !acquire()? {
            return Ok(None);
        }
        let publication = state.publication();
        *seen = publication.sequence;
        Ok(Some(publication))
    }

    /// An unavailable key has not consumed a picture. Retry on the dedicated
    /// presenter after a bounded sleep, even if no further source frame arrives.
    /// Condvar releases the mailbox lock and close/new publications wake early.
    fn wait_pending_retry(&self, observed_sequence: u64) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.active() && state.sequence == observed_sequence {
            drop(self.ready.wait_timeout(state, PENDING_RETRY_INTERVAL));
        }
    }
}

fn try_acquire(mutex: &IDXGIKeyedMutex, key: u64) -> Result<bool, String> {
    // AcquireSync returns WAIT_* as positive HRESULT values. The generated
    // Result treats those as success, so preserve the raw result here.
    let status = unsafe { (Interface::vtable(mutex).AcquireSync)(mutex.as_raw(), key, 0) };
    classify_acquire_status(status.0 as u32)
}

fn classify_acquire_status(status: u32) -> Result<bool, String> {
    match status {
        0 => Ok(true),
        0x102 => Ok(false),
        0x80 => Err("GPU preview shared texture mutex was abandoned.".into()),
        _ => Err(format!("GPU preview mutex failed: 0x{status:08X}")),
    }
}

pub(crate) fn prepare_shared(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    capture_bytes: u64,
) -> Result<Arc<SharedImage>, String> {
    validate_preview_payload(width, height, capture_bytes)?;
    create_shared(device, width, height).map(Arc::new)
}

fn validate_preview_payload(width: u32, height: u32, capture_bytes: u64) -> Result<(), String> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("GPU preview dimensions overflowed.")?;
    // Keep the prior conservative payload limit while restoring two BGRA flip
    // buffers. Capture's separate FP16 input/cache reservation remains intact;
    // this presenter needs no FP16 or DWM surface color intermediate.
    if capture_bytes.saturating_add(bytes.saturating_mul(5)) > MAX_PAYLOAD_BYTES {
        return Err("Native GPU preview exceeds the 512 MiB session payload budget.".into());
    }
    Ok(())
}

fn create_shared(device: &ID3D11Device, width: u32, height: u32) -> Result<SharedImage, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
            as u32,
    };
    let mut texture = None;
    // SAFETY: descriptor is initialized and the output is owned by this call.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|error| format!("GPU preview texture allocation failed: {error}"))?;
    let texture = texture.ok_or("GPU preview texture allocation returned none.")?;
    let mutex = texture
        .cast::<IDXGIKeyedMutex>()
        .map_err(|error| error.to_string())?;
    let resource = texture
        .cast::<IDXGIResource1>()
        .map_err(|error| error.to_string())?;
    // SAFETY: texture was allocated with the required NT/keyed flags.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )
    }
    .map_err(|error| format!("GPU preview handle creation failed: {error}"))?;
    let dxgi_device = device
        .cast::<IDXGIDevice>()
        .map_err(|error| error.to_string())?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.map_err(|error| error.to_string())?;
    Ok(SharedImage {
        diagnostic_id: NEXT_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed),
        texture,
        mutex,
        adapter,
        handle: handle.0 as usize,
        width,
        height,
    })
}

/// Presenter never uses the capture device's immediate context. `Present` may
/// wait or return a busy status only on this dedicated thread.
pub struct GpuPreviewPresenter {
    sink: Arc<GpuPreviewSink>,
    thread: Option<JoinHandle<()>>,
}

impl GpuPreviewPresenter {
    pub fn start(sink: &Arc<GpuPreviewSink>, hwnd: isize) -> Result<Self, String> {
        if hwnd == 0 {
            return Err("GPU preview has no child window.".into());
        }
        // Bound before capture admission; the HWND survives until close joins
        // this thread. Visibility queries never borrow the UI's D3D context.
        sink.hwnd.store(hwnd, Ordering::Release);
        let worker_sink = Arc::clone(sink);
        let thread = thread::Builder::new()
            .name("frame-gpu-preview".into())
            .spawn(move || {
                if let Err(error) = run_presenter(&worker_sink, HWND(hwnd as *mut c_void)) {
                    worker_sink.fail(error);
                }
            })
            .map_err(|error| format!("GPU preview presenter thread failed: {error}"))?;
        Ok(Self {
            sink: Arc::clone(sink),
            thread: Some(thread),
        })
    }

    pub fn close(&mut self) {
        self.sink.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GpuPreviewPresenter {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_presenter(sink: &GpuPreviewSink, hwnd: HWND) -> Result<(), String> {
    let mut seen = 0;
    let mut renderer = None;
    let mut gpu_clock = None;
    let mut gpu_clock_attempted = false;
    let mut last_presented_ns = None;
    while let Some((image, observed)) = sink.wait_next(seen) {
        if !sink.drawable() {
            // A hide can race the producer's admission. Discard the one pending
            // image without submitting a back-buffer copy; restoration admits
            // the next fresh capture frame rather than replaying a backlog.
            if let Some(publication) =
                sink.claim_current_with(&mut seen, || try_acquire(&image.mutex, 1))?
            {
                unsafe { image.mutex.ReleaseSync(0) }
                    .map_err(|error| format!("GPU preview hidden release failed: {error}"))?;
                diagnostics::record(
                    "preview_hidden_discard",
                    publication.frame.map(|f| f.capture),
                    || {
                        serde_json::json!({
                            "sink_id": sink.diagnostic_id, "generation": sink.generation,
                            "texture_id": image.diagnostic_id, "preview_sequence": publication.sequence,
                            "acquired_key": 1, "released_key": 0,
                        })
                    },
                );
            } else {
                diagnostics::record(
                    "preview_consumer_busy",
                    observed.frame.map(|f| f.capture),
                    || {
                        serde_json::json!({
                            "sink_id": sink.diagnostic_id, "generation": sink.generation,
                            "texture_id": image.diagnostic_id, "observed_sequence": observed.sequence,
                            "reason": "hidden_key_unavailable", "sequence_consumed": false,
                        })
                    },
                );
                sink.wait_pending_retry(observed.sequence);
            }
            continue;
        }
        if renderer.is_none() {
            renderer = Some(Renderer::new(&image, hwnd)?);
        }
        let renderer = renderer.as_ref().expect("renderer was initialized");
        if let Some(clock) = gpu_clock.as_mut() {
            gpu_timing::GpuClock::collect(clock, &renderer.context, &sink.counters);
        }
        if let Some(publication) =
            sink.claim_current_with(&mut seen, || try_acquire(&renderer.mutex, 1))?
        {
            let submitted_at_ns = publication.submitted_at_ns;
            let timed = (sink.timing_enabled.load(Ordering::Acquire) || diagnostics::enabled())
                && submitted_at_ns != 0;
            let started = timed.then(Instant::now);
            diagnostics::record(
                "preview_claim",
                publication.frame.map(|f| f.capture),
                || {
                    serde_json::json!({
                        "sink_id": sink.diagnostic_id, "generation": sink.generation,
                        "texture_id": image.diagnostic_id, "preview_sequence": publication.sequence,
                        "observed_sequence": observed.sequence, "acquired_key": 1,
                        "publication_age_ns": diagnostics::now_ns().saturating_sub(publication.published_ns),
                        "capture_age_ns": publication.frame.map(|f| diagnostics::now_ns().saturating_sub(f.capture.acquired_ns)),
                        "native_ticks": publication.frame.map(|f| f.native_ticks),
                        "media_ns": publication.frame.and_then(|f| f.media_ns),
                    })
                },
            );
            if timed && !gpu_clock_attempted {
                gpu_clock_attempted = true;
                gpu_clock = gpu_timing::GpuClock::new(&renderer.context);
                if gpu_clock.is_none() {
                    sink.counters
                        .gpu_timing_unavailable
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            let gpu_slot = if timed {
                gpu_clock
                    .as_mut()
                    .and_then(|clock| clock.begin(&renderer.context, &sink.counters))
            } else {
                None
            };
            if timed {
                let now = sink.origin.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                sink.counters
                    .mailbox_wait_cpu
                    .record(now.saturating_sub(submitted_at_ns));
            }
            renderer.copy_to_backbuffer_observed(Some((sink, &image, publication)))?;
            if let Some(index) = gpu_slot {
                gpu_clock
                    .as_mut()
                    .expect("slot has clock")
                    .end(&renderer.context, index);
            }
            // Present is isolated from capture and the UI thread, outside the
            // GPU copy timer. Its CPU submission remains timed separately.
            let present_started_ns = diagnostics::enabled().then(diagnostics::now_ns);
            let result = unsafe { renderer.swapchain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) };
            let present_finished_ns = present_started_ns.map(|_| diagnostics::now_ns());
            if let Some(started) = started {
                sink.counters
                    .presenter_submit_cpu
                    .record(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
            }
            diagnostics::record(
                "preview_present",
                publication.frame.map(|f| f.capture),
                || {
                    serde_json::json!({
                        "sink_id": sink.diagnostic_id, "generation": sink.generation,
                        "texture_id": image.diagnostic_id, "preview_sequence": publication.sequence,
                        "status": result.0, "presented": result.is_ok() && result != DXGI_ERROR_WAS_STILL_DRAWING,
                        "present_cpu_ns": present_finished_ns.zip(present_started_ns).map(|(end,start)| end.saturating_sub(start)),
                        "present_interval_ns": present_finished_ns.zip(last_presented_ns).map(|(end,start)| end.saturating_sub(start)),
                        "capture_to_present_submit_ns": present_finished_ns.zip(publication.frame).map(|(end,f)| end.saturating_sub(f.capture.acquired_ns)),
                        "completed_to_present_submit_ns": present_finished_ns.zip(publication.frame).map(|(end,f)| end.saturating_sub(f.completed_ns)),
                        "presenter_gpu_span": sink.counters.presenter_gpu_span.snapshot(),
                        "gpu_timing_unavailable": sink.counters.gpu_timing_unavailable.load(Ordering::Relaxed),
                        "gpu_timing_invalid": sink.counters.gpu_timing_invalid.load(Ordering::Relaxed),
                        "gpu_timing_busy": sink.counters.gpu_timing_busy.load(Ordering::Relaxed),
                        "gpu_timing_pending": gpu_clock.as_ref().map(gpu_timing::GpuClock::pending),
                        "gpu_span_scope": "asynchronous_copy_flush_commands_before_present_not_scanout",
                        "scanout_measured": false,
                    })
                },
            );
            if result == DXGI_ERROR_WAS_STILL_DRAWING {
                sink.counters.presenter_busy.fetch_add(1, Ordering::Relaxed);
            } else {
                result
                    .ok()
                    .map_err(|error| format!("GPU preview Present failed: {error}"))?;
                sink.counters.presented.fetch_add(1, Ordering::Relaxed);
            }
            if result != DXGI_ERROR_WAS_STILL_DRAWING {
                last_presented_ns = present_finished_ns;
            }
        } else {
            // The unavailable key has not consumed the pending publication.
            // Retry after sleeping even when this is the last source update.
            sink.counters.presenter_busy.fetch_add(1, Ordering::Relaxed);
            diagnostics::record(
                "preview_consumer_busy",
                observed.frame.map(|f| f.capture),
                || {
                    serde_json::json!({
                        "sink_id": sink.diagnostic_id, "generation": sink.generation,
                        "texture_id": image.diagnostic_id, "observed_sequence": observed.sequence,
                        "reason": "key_unavailable", "sequence_consumed": false,
                    })
                },
            );
            sink.wait_pending_retry(observed.sequence);
        }
    }
    if let (Some(clock), Some(renderer)) = (gpu_clock.as_mut(), renderer.as_ref()) {
        clock.collect(&renderer.context, &sink.counters);
        sink.counters
            .gpu_timing_unresolved
            .store(clock.pending(), Ordering::Relaxed);
    }
    Ok(())
}

// Retained offscreen regression fixture from the rejected surface experiment.
// Production uses the restored accepted BGRA HWND flip presenter below.
#[cfg(test)]
struct CompositionView {
    device: IDCompositionDevice,
    visual: IDCompositionVisual,
    surface: IDCompositionSurface,
    scale: IDCompositionScaleTransform,
    target: Option<IDCompositionTarget>,
    source: (u32, u32),
    client: Option<(i32, i32)>,
}

#[cfg(test)]
fn composition_scale(source: (u32, u32), client: (i32, i32)) -> Result<(f32, f32), String> {
    if source.0 == 0 || source.1 == 0 || client.0 <= 0 || client.1 <= 0 {
        return Err("GPU preview composition viewport is empty.".into());
    }
    // GPUI already aspect-fits this child to the source. Scale the entire
    // texture, including its far right/bottom edges, into those physical pixels.
    Ok((
        client.0 as f32 / source.0 as f32,
        client.1 as f32 / source.1 as f32,
    ))
}

#[cfg(test)]
impl CompositionView {
    fn new(device: &ID3D11Device, source: (u32, u32)) -> Result<Self, String> {
        let dxgi: IDXGIDevice = device.cast().map_err(|e| e.to_string())?;
        // SAFETY: all objects belong to this dedicated presenter's device.
        // Offscreen tests exercise this tree without an HWND.
        let (device, visual, surface, scale) = unsafe {
            let device: IDCompositionDevice = DCompositionCreateDevice(&dxgi)
                .map_err(|e| format!("GPU preview composition device failed: {e}"))?;
            let visual = device.CreateVisual().map_err(|e| e.to_string())?;
            let scale = device.CreateScaleTransform().map_err(|e| e.to_string())?;
            let surface = device
                .CreateSurface(
                    source.0,
                    source.1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_IGNORE,
                )
                .map_err(|e| format!("GPU preview composition surface failed: {e}"))?;
            visual.SetContent(&surface).map_err(|e| e.to_string())?;
            visual.SetTransform(&scale).map_err(|e| e.to_string())?;
            visual
                .SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)
                .map_err(|e| e.to_string())?;
            (device, visual, surface, scale)
        };
        Ok(Self {
            device,
            visual,
            surface,
            scale,
            target: None,
            source,
            client: None,
        })
    }

    fn resize(&mut self, client: (i32, i32)) -> Result<bool, String> {
        if self.client == Some(client) || client.0 <= 0 || client.1 <= 0 {
            return Ok(false);
        }
        let (x, y) = composition_scale(self.source, client)?;
        // SAFETY: live transform; the next frame's asynchronous Commit publishes
        // both geometry and pixels together. No DWM completion wait.
        unsafe {
            self.scale.SetScaleX2(x).map_err(|e| e.to_string())?;
            self.scale.SetScaleY2(y).map_err(|e| e.to_string())?;
        }
        self.client = Some(client);
        Ok(true)
    }

    fn copy_frame<T>(
        &self,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        inspect: impl FnOnce(&ID3D11Texture2D, u32, (u32, u32)) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut offset = POINT::default();
        // A full update also initializes the first frame. DWM may return an
        // atlas with a nonzero offset: never assume a source-sized texture or
        // retain the returned resource across EndDraw.
        let drawing: IDXGISurface = unsafe { self.surface.BeginDraw(None, &mut offset) }
            .map_err(|e| format!("GPU preview surface BeginDraw failed: {e}"))?;
        let result = (|| {
            let surface: IDXGISurface2 = drawing.cast().map_err(|e| e.to_string())?;
            let mut subresource = 0;
            let target: ID3D11Texture2D =
                unsafe { surface.GetResource(&mut subresource) }.map_err(|e| e.to_string())?;
            let mut src = D3D11_TEXTURE2D_DESC::default();
            let mut dst = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                source.GetDesc(&mut src);
                target.GetDesc(&mut dst);
            }
            if (src.Width, src.Height) != self.source {
                return Err("GPU preview source dimensions changed within one surface.".into());
            }
            let origin = surface_copy_origin(&src, &dst, subresource, offset)?;
            unsafe {
                context.CopySubresourceRegion(
                    &target,
                    subresource,
                    origin.0,
                    origin.1,
                    0,
                    source,
                    0,
                    Some(&D3D11_BOX {
                        left: 0,
                        top: 0,
                        front: 0,
                        right: src.Width,
                        bottom: src.Height,
                        back: 1,
                    }),
                );
            }
            inspect(&target, subresource, origin)
        })();
        // Balance every successful BeginDraw, including validation failures.
        // DWM's texture is neither cached nor mapped in production.
        drop(drawing);
        let ended = unsafe { self.surface.EndDraw() }
            .map_err(|e| format!("GPU preview surface EndDraw failed: {e}"));
        let value = result?;
        ended?;
        Ok(value)
    }
}

#[cfg(test)]
impl Drop for CompositionView {
    fn drop(&mut self) {
        if let Some(target) = &self.target {
            // Detach before the UI destroys its child. Do not wait for DWM.
            unsafe {
                let _ = target.SetRoot(None::<&IDCompositionVisual>);
                let _ = self.visual.SetContent(None::<&windows::core::IUnknown>);
                let _ = self.device.Commit();
            }
        }
    }
}

#[cfg(test)]
fn surface_copy_origin(
    source: &D3D11_TEXTURE2D_DESC,
    target: &D3D11_TEXTURE2D_DESC,
    subresource: u32,
    offset: POINT,
) -> Result<(u32, u32), String> {
    let x = u32::try_from(offset.x).map_err(|_| "GPU preview surface offset is negative.")?;
    let y = u32::try_from(offset.y).map_err(|_| "GPU preview surface offset is negative.")?;
    if source.Width == 0
        || source.Height == 0
        || source.Format != DXGI_FORMAT_B8G8R8A8_UNORM
        || target.Format != source.Format
        || source.MipLevels != 1
        || target.MipLevels != 1
        || source.ArraySize != 1
        || subresource >= target.ArraySize
        || source.SampleDesc.Count != 1
        || target.SampleDesc.Count != 1
        || source.SampleDesc.Quality != 0
        || target.SampleDesc.Quality != 0
        || x.checked_add(source.Width)
            .is_none_or(|right| right > target.Width)
        || y.checked_add(source.Height)
            .is_none_or(|bottom| bottom > target.Height)
    {
        return Err("GPU preview surface requires a valid BGRA update rectangle.".into());
    }
    Ok((x, y))
}

struct Renderer {
    context: ID3D11DeviceContext,
    shared: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    swapchain: IDXGISwapChain1,
}

impl Renderer {
    #[cfg(test)]
    fn present(&self) -> HRESULT {
        unsafe { self.swapchain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) }
    }

    /// Called only while this device owns key 1. A test reads this back buffer
    /// before Present; production never maps or copies it to the CPU.
    #[cfg(test)]
    fn copy_to_backbuffer(&self) -> Result<ID3D11Texture2D, String> {
        self.copy_to_backbuffer_observed(None)
    }

    fn copy_to_backbuffer_observed(
        &self,
        observation: Option<(&GpuPreviewSink, &SharedImage, Publication)>,
    ) -> Result<ID3D11Texture2D, String> {
        let buffer: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0) }
            .map_err(|error| format!("GPU preview back buffer failed: {error}"))?;
        let copy_start_ns = diagnostics::enabled().then(diagnostics::now_ns);
        unsafe { self.context.CopyResource(&buffer, &self.shared) };
        let copy_end_ns = copy_start_ns.map(|_| diagnostics::now_ns());
        // Microsoft requires a Flush after a cross-device shared texture
        // update. The keyed release then permits producer slot reuse.
        unsafe { self.context.Flush() };
        let flush_end_ns = copy_start_ns.map(|_| diagnostics::now_ns());
        unsafe { self.mutex.ReleaseSync(0) }
            .map_err(|error| format!("GPU preview consumer release failed: {error}"))?;
        if let Some((sink, image, publication)) = observation {
            diagnostics::record(
                "preview_backbuffer_copy",
                publication.frame.map(|f| f.capture),
                || {
                    serde_json::json!({
                        "sink_id": sink.diagnostic_id, "generation": sink.generation,
                        "texture_id": image.diagnostic_id, "preview_sequence": publication.sequence,
                        "copy_submit_ns": copy_end_ns.zip(copy_start_ns).map(|(end,start)| end.saturating_sub(start)),
                        "flush_ns": flush_end_ns.zip(copy_end_ns).map(|(end,start)| end.saturating_sub(start)),
                        "released_key": 0,
                    })
                },
            );
        }
        Ok(buffer)
    }

    fn new(image: &SharedImage, hwnd: HWND) -> Result<Self, String> {
        let mut device = None;
        let mut context = None;
        // SAFETY: adapter comes from the producer device, so this is the same
        // physical adapter and the device out-parameters are initialized.
        unsafe {
            D3D11CreateDevice(
                &image.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|error| format!("GPU preview presenter device failed: {error}"))?;
        let device: ID3D11Device = device.ok_or("GPU preview presenter device is missing.")?;
        let context = context.ok_or("GPU preview presenter context is missing.")?;
        let device1 = device
            .cast::<ID3D11Device1>()
            .map_err(|error| error.to_string())?;
        let shared: ID3D11Texture2D =
            unsafe { device1.OpenSharedResource1(HANDLE(image.handle as *mut c_void)) }
                .map_err(|error| format!("GPU preview shared texture open failed: {error}"))?;
        let mutex = shared
            .cast::<IDXGIKeyedMutex>()
            .map_err(|error| error.to_string())?;
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }
            .map_err(|error| format!("GPU preview DXGI factory failed: {error}"))?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: image.width,
            Height: image.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        // SAFETY: child HWND lives until the presenter thread is joined; the
        // flip chain back buffers remain on this presenter's D3D11 device.
        let swapchain = unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None) }
            .map_err(|error| format!("GPU preview swapchain failed: {error}"))?;
        Ok(Self {
            context,
            shared,
            mutex,
            swapchain,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SUBRESOURCE_DATA,
        D3D11_USAGE_STAGING,
    };
    use windows::Win32::UI::WindowsAndMessaging::WS_OVERLAPPEDWINDOW;

    #[test]
    fn preview_presentation_composition_maps_entire_desktop_to_resized_child() {
        let source = (2560, 1600);
        for client in [(1000, 625), (1250, 781), (2000, 1250), (640, 400)] {
            let (x, y) = composition_scale(source, client).unwrap();
            // Far edges must land on the child boundary, not beyond its clip.
            assert!((2560. * x - client.0 as f32).abs() < 0.001);
            assert!((1600. * y - client.1 as f32).abs() < 0.001);
        }
        assert!(composition_scale(source, (0, 0)).is_err());
        assert!(composition_scale((0, 1600), (1000, 625)).is_err());
    }

    #[test]
    fn preview_presentation_surface_copy_validates_atlas_offsets_and_format() {
        let src = D3D11_TEXTURE2D_DESC {
            Width: 4,
            Height: 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let dst = D3D11_TEXTURE2D_DESC {
            Width: 16,
            Height: 8,
            ..src
        };
        assert_eq!(
            surface_copy_origin(&src, &dst, 0, POINT { x: 12, y: 6 }).unwrap(),
            (12, 6)
        );
        for offset in [
            POINT { x: -1, y: 0 },
            POINT { x: 13, y: 6 },
            POINT { x: 12, y: 7 },
        ] {
            assert!(surface_copy_origin(&src, &dst, 0, offset).is_err());
        }
        assert!(surface_copy_origin(&src, &dst, 1, POINT::default()).is_err());
        let mismatch = D3D11_TEXTURE2D_DESC {
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
            ..dst
        };
        assert!(surface_copy_origin(&src, &mismatch, 0, POINT::default()).is_err());
        let overflow = D3D11_TEXTURE2D_DESC {
            Width: u32::MAX,
            ..src
        };
        assert!(surface_copy_origin(&overflow, &dst, 0, POINT { x: 1, y: 0 }).is_err());
    }

    #[test]
    fn preview_presentation_surface_preserves_full_sdr_pixels_without_a_window() {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .expect("offscreen hardware device");
        let device = device.unwrap();
        let context = context.unwrap();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 256,
            Height: 4,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let mut source = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut source)) }.unwrap();
        let source = source.unwrap();
        let mut view = CompositionView::new(&device, (256, 4)).unwrap();
        assert!(view.target.is_none()); // No HWND, attachment, Present, or desktop capture.
        assert!(view.resize((128, 2)).unwrap());
        assert!(!view.resize((128, 2)).unwrap());
        assert!(!view.resize((0, 0)).unwrap());
        assert!(view.resize((512, 8)).unwrap());
        let mut staging = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..desc
                },
                None,
                Some(&mut staging),
            )
        }
        .unwrap();
        let staging = staging.unwrap();
        for pass in 0..3u8 {
            let mut pixels = Vec::new();
            for y in 0..4 {
                for x in 0..256u16 {
                    let x = (x as u8).wrapping_add(pass * 37);
                    pixels.extend_from_slice(&match y {
                        0 => [x, x, x, 255],
                        1 => [x, 255 - x, 17, 255],
                        2 => [13, x, 201, 255],
                        _ => [253, 101, x, 255],
                    });
                }
            }
            unsafe {
                context.UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), 256 * 4, 0)
            };
            view.copy_frame(&context, &source, |target, subresource, origin| {
                // Test-only readback while the current BeginDraw resource is valid.
                unsafe {
                    context.CopySubresourceRegion(
                        &staging,
                        0,
                        0,
                        0,
                        0,
                        target,
                        subresource,
                        Some(&D3D11_BOX {
                            left: origin.0,
                            top: origin.1,
                            front: 0,
                            right: origin.0 + 256,
                            bottom: origin.1 + 4,
                            back: 1,
                        }),
                    );
                }
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.unwrap();
                let actual: Vec<u8> = (0..4)
                    .flat_map(|y| unsafe {
                        std::slice::from_raw_parts(
                            mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                            256 * 4,
                        )
                        .to_vec()
                    })
                    .collect();
                unsafe { context.Unmap(&staging, 0) };
                assert_eq!(
                    actual, pixels,
                    "full frame and every neutral/channel value, update {pass}"
                );
                Ok(())
            })
            .unwrap();
        }
        // Validation failures must still close BeginDraw; a fresh valid update succeeds.
        let mut invalid = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC { Width: 257, ..desc },
                None,
                Some(&mut invalid),
            )
        }
        .unwrap();
        assert!(
            view.copy_frame(&context, &invalid.unwrap(), |_, _, _| Ok(()))
                .is_err()
        );
        view.copy_frame(&context, &source, |_, _, _| Ok(()))
            .unwrap();
    }

    fn publish_identity(sink: &GpuPreviewSink, sequence: u64) {
        let mut state = sink.state.lock().unwrap();
        state.sequence = sequence;
        state.submitted_at_ns = sequence * 100;
        state.published_ns = sequence * 200;
        state.frame = Some(PreviewFrameTrace {
            capture: crate::cursor_preview_diagnostics::CaptureTrace {
                source_id: 43,
                sequence: sequence + 1000,
                acquired_ns: sequence * 50,
            },
            native_ticks: (sequence * 10) as i64,
            media_ns: Some(sequence * 75),
            completed_ns: sequence * 90,
        });
    }

    #[test]
    fn pending_overwrite_claims_current_pixels_and_publication_together() {
        // Deterministic CPU interleaving at the production claim boundary. The
        // acquire closure controls key availability; this is not pixel readback
        // or a reproduction of the owner's visible doubled cursor.
        let sink = GpuPreviewSink::new(101);
        publish_identity(&sink, 1);
        let old_observation = sink.state.lock().unwrap().publication();
        // A producer replaces the pending image after wait_next's observation,
        // before the presenter owns the resource. The old path used identity 1.
        publish_identity(&sink, 2);
        let mut seen = 0;
        let claimed = sink
            .claim_current_with(&mut seen, || {
                // Producer try_lock cannot mutate the publication while the
                // consumer acquires the corresponding keyed texture.
                assert!(matches!(
                    sink.state.try_lock(),
                    Err(TryLockError::WouldBlock)
                ));
                Ok(true)
            })
            .unwrap()
            .unwrap();
        assert_eq!(old_observation.sequence, 1);
        assert_ne!(old_observation.sequence, claimed.sequence);
        assert_eq!(claimed.sequence, 2);
        assert_eq!(claimed.submitted_at_ns, 200);
        assert_eq!(claimed.published_ns, 400);
        assert_eq!(claimed.frame.unwrap().capture.sequence, 1002);
        assert_eq!(claimed.frame.unwrap().native_ticks, 20);
        assert_eq!(claimed.frame.unwrap().media_ns, Some(150));
        assert_eq!(seen, 2);
        // A consumed publication cannot acquire/present the same pixels again.
        assert!(
            sink.claim_current_with(&mut seen, || panic!("already consumed"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn busy_key_retains_last_publication_until_ready_without_new_publish() {
        let sink = GpuPreviewSink::new(102);
        publish_identity(&sink, 7);
        let mut seen = 0;
        assert!(
            sink.claim_current_with(&mut seen, || Ok(false))
                .unwrap()
                .is_none()
        );
        // The former timeout branch assigned seen=7 and wait_next would wait
        // for a new sequence. GPU/key readiness itself is not a publication.
        assert_eq!(seen, 0);
        assert_ne!(sink.state.lock().unwrap().sequence, seen);
        sink.wait_pending_retry(7);
        let claimed = sink
            .claim_current_with(&mut seen, || Ok(true))
            .unwrap()
            .unwrap();
        assert_eq!(sink.state.lock().unwrap().sequence, 7);
        assert_eq!(claimed.sequence, 7);
        assert_eq!(claimed.frame.unwrap().capture.sequence, 1007);
        assert_eq!(seen, 7);
    }

    #[test]
    fn failed_or_closed_claim_does_not_consume_or_acquire_a_publication() {
        let sink = GpuPreviewSink::new(103);
        publish_identity(&sink, 4);
        let mut seen = 0;
        assert_eq!(
            sink.claim_current_with(&mut seen, || Err("device failure".into()))
                .err()
                .as_deref(),
            Some("device failure")
        );
        assert_eq!(seen, 0);
        sink.close();
        sink.wait_pending_retry(4);
        assert!(
            sink.claim_current_with(&mut seen, || panic!("closed sink acquired a key"))
                .unwrap()
                .is_none()
        );
        assert_eq!(seen, 0);
    }

    #[test]
    fn producer_failure_does_not_wait_for_the_presenter_mailbox() {
        let sink = GpuPreviewSink::new(91);
        let _held = sink.state.lock().unwrap();
        sink.fail("preview driver failed".into());
        assert!(!sink.active());
        assert_eq!(sink.error().as_deref(), Some("preview driver failed"));
    }

    #[test]
    fn refresh_cap_is_independent_and_does_not_accumulate_catch_up_work() {
        let sink = GpuPreviewSink::new(11);
        let interval = refresh_interval_nanos(DEFAULT_GPU_PREVIEW_REFRESH_HZ).unwrap();
        assert!(sink.admit_at(Duration::ZERO, true));
        assert!(!sink.admit_at(Duration::from_nanos(interval - 1), true));
        assert!(sink.admit_at(Duration::from_nanos(interval), true));
        // A pause admits one newest image, not all the missed refreshes.
        assert!(sink.admit_at(Duration::from_secs(5), true));
        assert!(!sink.admit_at(Duration::from_secs(5), true));
        let stats = sink.stats();
        assert_eq!(stats.frames_considered, 5);
        assert_eq!(stats.rate_limited, 2);
        assert_eq!(stats.copied, 0);
        assert_eq!(
            crate::preview::PREVIEW_MIN_INTERVAL,
            Duration::from_millis(100)
        );
    }

    #[test]
    fn preview_refresh_rate_is_validated_and_bounds_high_capture_cadence() {
        let sink = GpuPreviewSink::new(12);
        assert!(sink.set_refresh_rate(0).is_err());
        assert!(sink.set_refresh_rate(61).is_err());
        sink.set_refresh_rate(20).unwrap();
        // A 1,000 FPS completed-source feed performs at most twenty preview
        // attempts in a one-second half-open interval, regardless of backlog.
        let admitted = (0..1_000)
            .filter(|millis| sink.admit_at(Duration::from_millis(*millis), true))
            .count();
        assert_eq!(admitted, 20);
        assert_eq!(sink.stats().rate_limited, 980);
        // Increasing the interval applies immediately to the previous attempt.
        sink.set_refresh_rate(1).unwrap();
        assert!(!sink.admit_at(Duration::from_millis(1_000), true));
        assert!(sink.admit_at(Duration::from_millis(1_950), true));
    }

    #[test]
    fn suspended_preview_admits_fresh_frame_on_restore_and_close_stays_terminal() {
        let sink = GpuPreviewSink::new(13);
        assert!(!sink.admit_at(Duration::ZERO, false));
        assert!(sink.admit_at(Duration::ZERO, true));
        assert!(!sink.admit_at(Duration::from_secs(10), false));
        assert!(sink.admit_at(Duration::from_secs(10), true));
        assert!(!sink.admit_at(Duration::from_secs(10), true));
        assert_eq!(sink.stats().suspended, 2);
        sink.close();
        assert!(!sink.admit_at(Duration::from_secs(11), true));
        assert!(!sink.active());
    }

    #[test]
    fn close_wakes_a_presenter_with_no_pending_frames() {
        let sink = GpuPreviewSink::new(15);
        let waiter_sink = Arc::clone(&sink);
        let (finished, result) = std::sync::mpsc::sync_channel(1);
        let waiter = thread::spawn(move || {
            finished.send(waiter_sink.wait_next(0).is_none()).unwrap();
        });
        sink.close();
        assert!(result.recv_timeout(Duration::from_secs(2)).unwrap());
        waiter.join().unwrap();
    }

    #[test]
    fn native_visibility_suspends_hidden_minimized_and_destroyed_windows() {
        use windows::Win32::UI::WindowsAndMessaging::WS_MINIMIZE;
        assert!(!window_drawable(0));
        // The parent is deliberately hidden; no production UI is launched.
        let parent = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW | WS_MINIMIZE,
                0,
                0,
                96,
                64,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let child = create_child_window(parent.0 as isize).unwrap();
        place_child_window(child, (0, 0, 64, 32), true).unwrap();
        assert!(unsafe { IsIconic(parent) }.as_bool());
        assert!(!window_drawable(child));
        let sink = GpuPreviewSink::new(14);
        sink.hwnd.store(child, Ordering::Release);
        assert!(!sink.admit_frame());
        assert_eq!(sink.stats().suspended, 1);
        assert!(sink.state.lock().unwrap().image.is_none());
        destroy_child_window(child).unwrap();
        assert!(!window_drawable(child));
        unsafe { DestroyWindow(parent) }.unwrap();
    }

    #[test]
    fn acquire_status_distinguishes_success_timeout_and_abandoned() {
        assert!(classify_acquire_status(0).unwrap());
        assert!(!classify_acquire_status(0x102).unwrap());
        assert!(
            classify_acquire_status(0x80)
                .unwrap_err()
                .contains("abandoned")
        );
        assert!(classify_acquire_status(0x8000_4005).is_err());
    }

    #[test]
    fn synthetic_bgra_handoff_busy_slot_and_detach() {
        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 32;
        // This test owns its hidden HWND and synthetic D3D11 texture. It never
        // opens a display/camera/microphone or touches the running application.
        let parent = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                96,
                64,
                None,
                None,
                None,
                None,
            )
        }
        .expect("synthetic parent HWND");
        let child = create_child_window(parent.0 as isize).expect("synthetic child HWND");
        place_child_window(child, (0, 0, 64, 32), false).unwrap();

        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .expect("synthetic producer D3D11 device");
        let device = device.unwrap();
        let context = context.unwrap();
        let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        let colors = [
            [0, 0, 255, 255],     // red
            [0, 255, 0, 255],     // green
            [255, 0, 0, 255],     // blue
            [255, 255, 255, 255], // white
        ];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let quadrant = usize::from(x >= WIDTH / 2) + 2 * usize::from(y >= HEIGHT / 2);
                let offset = ((y * WIDTH + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&colors[quadrant]);
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: WIDTH * 4,
            SysMemSlicePitch: (WIDTH * HEIGHT * 4),
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }
            .expect("synthetic source texture");
        let source = PreviewSource::new(&texture.unwrap(), WIDTH, HEIGHT);
        let hub = crate::preview::new_hub(crate::preview::PreviewConfig::default(), 41).unwrap();
        let sink = GpuPreviewSink::new(41);
        hub.set_gpu_sink(Some(Arc::clone(&sink))).unwrap();
        assert!(hub.set_gpu_sink(Some(GpuPreviewSink::new(42))).is_err());
        let prepared = prepare_shared(&device, WIDTH, HEIGHT, 0).unwrap();
        assert!(sink.prepare_image(&prepared));
        sink.submit(&source, &device, &context, 0).unwrap();
        let image = Arc::clone(sink.state.lock().unwrap().image.as_ref().unwrap());
        let renderer =
            Renderer::new(&image, HWND(child as *mut c_void)).expect("presenter device/surface");

        assert!(try_acquire(&renderer.mutex, 1).unwrap());
        let sequence = sink.state.lock().unwrap().sequence;
        // Presenter owns the shared image. A capture submission must skip it
        // immediately, without queueing another GPU copy or changing sequence.
        sink.submit(&source, &device, &context, 0).unwrap();
        assert_eq!(sink.state.lock().unwrap().sequence, sequence);
        assert_eq!(sink.stats().producer_busy, 1);
        let buffer = renderer.copy_to_backbuffer().unwrap();

        // Reuse the released shared slot with a different image before the
        // first back buffer is mapped. The first back buffer must keep its
        // original quadrants across the two device command streams.
        let replacement_pixels = vec![255u8; (WIDTH * HEIGHT * 4) as usize];
        let replacement_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: replacement_pixels.as_ptr().cast(),
            SysMemPitch: WIDTH * 4,
            SysMemSlicePitch: WIDTH * HEIGHT * 4,
        };
        let mut replacement_texture = None;
        unsafe {
            device.CreateTexture2D(
                &desc,
                Some(&replacement_data),
                Some(&mut replacement_texture),
            )
        }
        .unwrap();
        let replacement = PreviewSource::new(&replacement_texture.unwrap(), WIDTH, HEIGHT);
        sink.submit(&replacement, &device, &context, 0).unwrap();
        assert_eq!(sink.state.lock().unwrap().sequence, sequence + 1);
        assert_eq!(sink.stats().copied, 2);

        // A pending picture is replaced in the same shared texture, without
        // allocating another slot or retaining a recording surface.
        sink.submit(&source, &device, &context, 0).unwrap();
        assert_eq!(sink.state.lock().unwrap().sequence, sequence + 2);
        assert_eq!(sink.stats().replaced_pending, 1);
        assert!(Arc::ptr_eq(
            sink.state.lock().unwrap().image.as_ref().unwrap(),
            &image,
        ));

        // A UI-held state lock drops immediately; it cannot backpressure capture.
        let held_state = sink.state.lock().unwrap();
        sink.submit(&source, &device, &context, 0).unwrap();
        assert_eq!(held_state.sequence, sequence + 2);
        assert_eq!(sink.stats().producer_busy, 2);
        drop(held_state);

        // Fixture-only readback checks a stable BGRA snapshot of the production
        // surface copy after the same cross-device keyed handoff.
        let presenter_device = unsafe { renderer.context.GetDevice() }.unwrap();
        let mut staging = None;
        unsafe {
            presenter_device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..desc
                },
                None,
                Some(&mut staging),
            )
        }
        .unwrap();
        let staging = staging.unwrap();
        unsafe { renderer.context.CopyResource(&staging, &buffer) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            renderer
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .unwrap();
        for (x, y, expected) in [
            (8usize, 8usize, colors[0]),
            (56, 8, colors[1]),
            (8, 24, colors[2]),
            (56, 24, colors[3]),
        ] {
            let offset = y * mapped.RowPitch as usize + x * 4;
            let actual =
                unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(offset), 4) };
            assert_eq!(actual, expected);
        }
        unsafe { renderer.context.Unmap(&staging, 0) };
        let presented = renderer.present();
        assert!(presented.is_ok() || presented == DXGI_ERROR_WAS_STILL_DRAWING);
        assert!(try_acquire(&image.mutex, 1).unwrap());
        unsafe { image.mutex.ReleaseSync(0) }.unwrap();
        hub.set_gpu_sink(None).unwrap();
        assert!(!sink.active());
        sink.submit(&source, &device, &context, 0).unwrap();
        assert_eq!(sink.state.lock().unwrap().sequence, sequence + 2);
        drop(renderer);
        destroy_child_window(child).unwrap();
        unsafe { DestroyWindow(parent) }.unwrap();
    }

    #[test]
    fn format_color_preview_budget_retains_conservative_surface_reservation() {
        let source_bytes = 2560_u64 * 1600 * 4;
        let capture_bytes = source_bytes * 5;
        assert!(validate_preview_payload(2560, 1600, capture_bytes).is_ok());
        // Source-owned slots/cache/FP16 input are reserved in addition to the
        // producer shared image and conservative DWM surface allowance.
        let capture_bytes = 3840_u64 * 2160 * 28;
        assert!(validate_preview_payload(3840, 2160, capture_bytes).is_ok());
        assert!(validate_preview_payload(4096, 4096, 4096_u64 * 4096 * 28).is_err());
        assert!(validate_preview_payload(u32::MAX, u32::MAX, 0).is_err());
    }
}
