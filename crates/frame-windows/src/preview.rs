#![allow(unsafe_code)] // Isolated D3D11 readback boundary with checked bounds.
//! Bounded live-preview images from the *same* capture session surface
//! (FRAME-0208).
//!
//! This module answers one question: how does the desktop UI see a real picture
//! without a second display capture?
//!
//! - **Same session, same device.** [`ThumbnailGenerator`] is created by the capture
//!   worker that already owns the D3D11 device, the immediate context and the four
//!   preallocated capture textures (ADR 0005). It opens no duplication, no device and
//!   no texture of its own beyond the reduction intermediate, a small render target
//!   and a small staging texture. It copies from the texture slot the capture session
//!   just completed.
//!   There is no second display capture, no cross-device sharing, and no COM handle in
//!   any public type.
//! - **Fit on the GPU, read back the selected result.** Native images copy directly
//!   into staging. Reduced images pass through a Frame-owned intermediate and
//!   `VideoProcessorBlt` before their staging copy.
//!   `Map` uses `DO_NOT_WAIT`, and only the selected image reaches the CPU. No
//!   shader, sampler or full-frame CPU staging is used; the format is BGRA8.
//! - **One latest pending frame.** [`PreviewHub`] holds exactly one slot. A newer image
//!   replaces an older one that no consumer has taken yet
//!   ([`PreviewCounterSnapshot::replaced_not_taken`]); it can never grow into a
//!   history, and it can never displace, delay or fail a recording frame. The capture
//!   worker's preview step cannot return an error: a preview failure is published as
//!   [`PreviewImage::error`] and the recording continues.
//! - **Work stops when nobody is watching.** Every generation is gated on an attached
//!   [`PreviewSubscriber`]; with no subscriber the worker performs no GPU copy, no map,
//!   no encode and no allocation ([`PreviewCounterSnapshot::skipped_no_subscriber`]).
//! - **One in-flight frame.** [`PreviewHub::take`] hands out at most one image until
//!   the consumer acknowledges it, so a slow UI cannot build a queue anywhere between
//!   the worker and the frontend.
//!
//! The native capture source draws a separate hardware cursor onto the shared
//! surface before this module reads it back. This module does not scale up or
//! carry audio. Reported numbers include capture-thread readback and CPU-worker
//! encoding costs of performed work.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use flate2::{Compression, write::ZlibEncoder};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
use windows_core::Interface;

use crate::capture::diagnostics::Counters;
use crate::surface::MAX_PAYLOAD_BYTES;

/// Largest admitted preview dimension; smaller sources are never upscaled.
pub const PREVIEW_MAX_WIDTH: u32 = 4096;
/// Largest admitted preview dimension; the source aspect ratio is preserved.
pub const PREVIEW_MAX_HEIGHT: u32 = 4096;
/// Shortest accepted interval between two preview images: at most 10 fps.
pub const PREVIEW_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Holds a 4096-square RGB PNG even if its deflate stream is incompressible.
/// The hub and IPC still allow only one unacknowledged image, not a frame history.
pub const MAX_PREVIEW_IMAGE_BYTES: usize = 72 * 1024 * 1024;
/// The image format the frontend receives. The frontend never processes pixels.
pub const PREVIEW_IMAGE_MIME: &str = "image/png";

/// One bounded shelf still; never opens a second DXGI duplication or changes
/// the recorder's source. Selected sources reuse their GPU image. Other outputs
/// use a small desktop-compositor still, with no capture session or polling loop.
pub fn source_thumbnail(
    target: &frame_contracts::CaptureTarget,
    cancelled: &std::sync::atomic::AtomicBool,
    shared: Option<&crate::gpu_preview::GpuPreviewSink>,
) -> Result<Vec<u8>, String> {
    static SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SLOT
        .try_lock()
        .map_err(|_| "Preview is busy. Hover again.".to_owned())?;
    if cancelled.load(Ordering::Relaxed) {
        return Err("Preview cancelled".into());
    }
    if let Some(shared) = shared {
        return shared.thumbnail(cancelled);
    }
    let frame_contracts::CaptureTarget::Display(id) = target else {
        return Err("Only display thumbnails are available".into());
    };
    let display = crate::display::enumerate_displays()
        .map_err(|e| e.message)?
        .into_iter()
        .find(|display| &display.id == id)
        .ok_or("Display is no longer available")?;
    crate::display::with_per_monitor_awareness(|| desktop_still(&display, cancelled))
        .map_err(|e| e.message)
}

fn desktop_still(
    display: &frame_contracts::DisplayInfo,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, RecorderError> {
    use windows::Win32::Graphics::Gdi::*;
    // Every DC/bitmap is confined to this background worker and released on all exits.
    struct Still {
        screen: HDC,
        memory: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
    }
    impl Drop for Still {
        fn drop(&mut self) {
            unsafe {
                if !self.memory.is_invalid() {
                    if !self.previous.is_invalid() {
                        let _ = SelectObject(self.memory, self.previous);
                    }
                    let _ = DeleteDC(self.memory);
                }
                if !self.bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
                }
                if !self.screen.is_invalid() {
                    let _ = ReleaseDC(None, self.screen);
                }
            }
        }
    }
    if display.width == 0
        || display.height == 0
        || display.width > i32::MAX as u32
        || display.height > i32::MAX as u32
        || cancelled.load(Ordering::Relaxed)
    {
        return Err(internal("Display preview cancelled or unavailable."));
    }
    let scale = (280. / display.width as f64)
        .min(158. / display.height as f64)
        .min(1.);
    let width = (display.width as f64 * scale).round().max(1.) as i32;
    let height = (display.height as f64 * scale).round().max(1.) as i32;
    let mut still = Still {
        screen: unsafe { GetDC(None) },
        memory: HDC::default(),
        bitmap: HBITMAP::default(),
        previous: HGDIOBJ::default(),
    };
    if still.screen.is_invalid() {
        return Err(internal("Desktop preview DC unavailable."));
    }
    still.memory = unsafe { CreateCompatibleDC(Some(still.screen)) };
    if still.memory.is_invalid() {
        return Err(internal("Desktop preview DC allocation failed."));
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: checked small top-down BGRA8 DIB; its storage outlives encoding.
    still.bitmap = unsafe {
        CreateDIBSection(
            Some(still.memory),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
    }
    .map_err(|e| internal(e.to_string()))?;
    if bits.is_null() {
        return Err(internal("Desktop preview returned no storage."));
    }
    still.previous = unsafe { SelectObject(still.memory, HGDIOBJ(still.bitmap.0)) };
    if still.previous.is_invalid() {
        return Err(internal("Desktop preview bitmap selection failed."));
    }
    unsafe {
        SetStretchBltMode(still.memory, HALFTONE);
    }
    // SAFETY: source uses observed physical monitor bounds inside the existing
    // per-monitor DPI guard. Only the reduced destination enters CPU memory.
    if !unsafe {
        StretchBlt(
            still.memory,
            0,
            0,
            width,
            height,
            Some(still.screen),
            display.x,
            display.y,
            display.width as i32,
            display.height as i32,
            SRCCOPY | CAPTUREBLT,
        )
    }
    .as_bool()
        || !unsafe { GdiFlush() }.as_bool()
    {
        return Err(internal("Desktop preview copy failed."));
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(internal("Display preview cancelled."));
    }
    let pixels = unsafe {
        std::slice::from_raw_parts(bits.cast::<u8>(), width as usize * height as usize * 4)
    };
    let mut png = Vec::new();
    encode_png(pixels, width as u32, height as u32, &mut png)?;
    Ok(png)
}

/// The explicit preview request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewConfig {
    /// Native means an oversized source is rejected, never reduced silently.
    pub native: bool,
    /// Largest image width this session may produce.
    pub max_width: u32,
    /// Largest image height this session may produce.
    pub max_height: u32,
    /// Smallest accepted interval between two images.
    pub min_interval: Duration,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            native: true,
            max_width: PREVIEW_MAX_WIDTH,
            max_height: PREVIEW_MAX_HEIGHT,
            min_interval: PREVIEW_MIN_INTERVAL,
        }
    }
}

impl PreviewConfig {
    pub fn reduced(max_width: u32, max_height: u32) -> Self {
        Self {
            native: false,
            max_width,
            max_height,
            ..Self::default()
        }
    }
    /// Validates the request before anything is allocated.
    ///
    /// A larger-than-allowed image, a zero dimension or an interval below the 10 fps
    /// bound is refused with `InvalidRequest`; nothing is clamped silently.
    pub fn validate(&self) -> Result<(), RecorderError> {
        if self.max_width == 0 || self.max_height == 0 {
            return Err(invalid_request(format!(
                "A preview image of {}x{} has no pixels.",
                self.max_width, self.max_height
            )));
        }
        if self.max_width > PREVIEW_MAX_WIDTH || self.max_height > PREVIEW_MAX_HEIGHT {
            return Err(invalid_request(format!(
                "A {}x{} preview exceeds the {PREVIEW_MAX_WIDTH}x{PREVIEW_MAX_HEIGHT} bound; Frame does not clamp a requested preview size silently.",
                self.max_width, self.max_height
            )));
        }
        if self.min_interval < PREVIEW_MIN_INTERVAL {
            return Err(invalid_request(format!(
                "A {} ms preview interval is faster than the {} ms (10 fps) bound.",
                self.min_interval.as_millis(),
                PREVIEW_MIN_INTERVAL.as_millis()
            )));
        }
        Ok(())
    }
}

/// Everything one preview image carries.
#[derive(Clone)]
pub struct PreviewImage {
    /// Image width, at most [`PreviewConfig::max_width`].
    pub width: u32,
    /// Image height, at most [`PreviewConfig::max_height`].
    pub height: u32,
    /// Monotonic sequence of this image *within this hub*.
    pub sequence: u64,
    /// `true` for the first image of this hub, so a consumer can start a new source
    /// instead of painting the previous source's picture into the new one.
    pub first_of_session: bool,
    /// The media timestamp of the capture surface this image was produced from, or
    /// `None` for an image that reports a failure instead of pixels.
    pub source_media_ns: Option<u64>,
    /// Walks this hub's lifetime, so a consumer discards an image that belongs to an
    /// earlier source instead of presenting it as the current one.
    pub session_generation: u64,
    /// The image format, for the consumer's `data:` URL. Never a second format.
    pub mime_type: &'static str,
    /// Either the encoded image or the bounded failure text. Never both.
    pub data: Result<Vec<u8>, String>,
}

impl PreviewImage {
    /// The encoded image, or `None` when this image reports a failure.
    pub fn encoded(&self) -> Option<&[u8]> {
        self.data.as_ref().ok().map(Vec::as_slice)
    }

    /// The failure text, or `None` when this image carries pixels.
    pub fn error(&self) -> Option<&str> {
        self.data.as_ref().err().map(String::as_str)
    }
}

impl std::fmt::Debug for PreviewImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("sequence", &self.sequence)
            .field("first_of_session", &self.first_of_session)
            .field("source_media_ns", &self.source_media_ns)
            .field("session_generation", &self.session_generation)
            .field("mime_type", &self.mime_type)
            .field(
                "data",
                &match &self.data {
                    Ok(bytes) => format!("{} encoded bytes", bytes.len()),
                    Err(error) => format!("error: {error}"),
                },
            )
            .finish()
    }
}

/// Worker-side preview counters. Kept separate from the capture counters: preview
/// activity is never mixed into the capture path's accounting identities.
#[derive(Default)]
pub struct PreviewCounters {
    pub(crate) generated: AtomicU64,
    pub(crate) published: AtomicU64,
    pub(crate) replaced_not_taken: AtomicU64,
    pub(crate) publication_contention: AtomicU64,
    pub(crate) skipped_no_subscriber: AtomicU64,
    pub(crate) rate_limited: AtomicU64,
    pub(crate) map_busy: AtomicU64,
    pub(crate) failures: AtomicU64,
    pub(crate) readback_nanos: AtomicU64,
    pub(crate) encode_nanos: AtomicU64,
}

/// One plain copy of [`PreviewCounters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreviewCounterSnapshot {
    /// Images the worker actually produced (GPU readback plus encode).
    pub generated: u64,
    /// Images handed to the hub.
    pub published: u64,
    /// Untaken images a newer image replaced: the bounded-drop evidence.
    pub replaced_not_taken: u64,
    /// Images skipped because publishing would have waited for a consumer.
    pub publication_contention: u64,
    /// Completed surfaces for which no generation was attempted: no subscriber.
    pub skipped_no_subscriber: u64,
    /// Completed surfaces skipped by the 10 fps bound.
    pub rate_limited: u64,
    /// Staging-resource polls that found the GPU still writing.
    pub map_busy: u64,
    /// Generations that failed and were published as an error image.
    pub failures: u64,
    /// Total GPU copy plus staging readback time inside the capture worker.
    pub readback_nanos: u64,
    /// Total PNG encode time on the preview CPU worker for desktop capture.
    pub encode_nanos: u64,
}

impl PreviewCounterSnapshot {
    /// Combined preview work time across capture readback and PNG encoding.
    pub fn total_nanos(&self) -> u64 {
        self.readback_nanos.saturating_add(self.encode_nanos)
    }

    /// True when this session did no preview work at all.
    pub fn is_idle(&self) -> bool {
        self.generated == 0
            && self.skipped_no_subscriber == 0
            && self.rate_limited == 0
            && self.failures == 0
    }
}

impl PreviewCounters {
    pub(crate) fn snapshot(&self) -> PreviewCounterSnapshot {
        PreviewCounterSnapshot {
            generated: self.generated.load(Ordering::Relaxed),
            published: self.published.load(Ordering::Relaxed),
            replaced_not_taken: self.replaced_not_taken.load(Ordering::Relaxed),
            publication_contention: self.publication_contention.load(Ordering::Relaxed),
            skipped_no_subscriber: self.skipped_no_subscriber.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
            map_busy: self.map_busy.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            readback_nanos: self.readback_nanos.load(Ordering::Relaxed),
            encode_nanos: self.encode_nanos.load(Ordering::Relaxed),
        }
    }
}

/// Cost of the last generated preview image, for the diagnostic report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreviewCost {
    pub(crate) readback_nanos: u64,
    pub(crate) encode_nanos: u64,
    pub(crate) image_width: u32,
    pub(crate) image_height: u32,
    pub(crate) encoded_bytes: usize,
}

/// The last preview image a session published, for diagnostics only.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreviewSlotState {
    pub(crate) cost: PreviewCost,
    pub(crate) media_nanos: Option<u64>,
}

/// The one latest pending preview image plus its transport state.
struct PreviewSlot {
    /// `None` until an image is published; a published failure is `Some(Err(..))`.
    image: Option<PreviewImage>,
    /// `true` while a consumer holds an unacknowledged image.
    taken: bool,
    /// Sequence of the next published image.
    next_sequence: u64,
    /// Images published by this hub, so the first one can be marked.
    published_total: u64,
    /// Walked when a hub is attached to a session.
    generation: u64,
}

/// Creates one preview hub for a validated request.
///
/// This is the host's seam: a host that owns the capture session creates the hub and
/// attaches it with [`crate::WindowsCaptureSession::attach_preview`]. The request is
/// validated here, so a hub can never exist outside the documented bounds, and the
/// returned hub is usable only through the same one-slot, one-in-flight rules the
/// capture worker publishes into.
pub fn new_hub(config: PreviewConfig, generation: u64) -> Result<Arc<PreviewHub>, RecorderError> {
    config.validate()?;
    Ok(PreviewHub::new(config, generation))
}

/// The one latest preview image, shared by the capture worker and the UI transport.
///
/// Creating a hub is cheap: a mutex-guarded slot and counters, no texture, no thread and
/// no allocation. Attaching it to a capture session stores an `Arc` only; the worker
/// allocates the two small preview textures lazily on the first image it generates.
pub struct PreviewHub {
    slot: Mutex<PreviewSlot>,
    ready: Condvar,
    subscribers: AtomicI64,
    enabled: AtomicBool,
    generation: AtomicU64,
    subscription_epoch: AtomicU64,
    config: PreviewConfig,
    counters: Arc<PreviewCounters>,
    gpu: Mutex<Option<Arc<crate::gpu_preview::GpuPreviewSink>>>,
    recording: AtomicBool,
    camera_warning: Mutex<Option<String>>,
}

impl std::fmt::Debug for PreviewHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewHub")
            .field("config", &self.config)
            .field("subscribers", &self.subscriber_count())
            .field("enabled", &self.enabled())
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl PreviewHub {
    /// Creates one hub for a validated request.
    pub(crate) fn new(config: PreviewConfig, generation: u64) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(PreviewSlot {
                image: None,
                taken: false,
                next_sequence: 1,
                published_total: 0,
                generation,
            }),
            ready: Condvar::new(),
            subscribers: AtomicI64::new(0),
            enabled: AtomicBool::new(true),
            generation: AtomicU64::new(generation),
            subscription_epoch: AtomicU64::new(0),
            config,
            counters: Arc::new(PreviewCounters::default()),
            gpu: Mutex::new(None),
            recording: AtomicBool::new(false),
            camera_warning: Mutex::new(None),
        })
    }

    /// Optional-source failures do not replace the desktop image or poison its sink.
    pub fn camera_warning(&self) -> Option<String> {
        self.camera_warning.lock().ok().and_then(|s| s.clone())
    }
    pub(crate) fn report_camera_warning(&self, message: String) {
        if let Ok(mut slot) = self.camera_warning.lock() {
            *slot = Some(message);
        }
    }
    /// The request this hub was created with.
    pub fn config(&self) -> PreviewConfig {
        self.config
    }

    /// The source generation this hub currently carries.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Selects the experimental GPU transport for this hub. While attached,
    /// the capture worker never creates another PNG frame for this source.
    pub fn set_gpu_sink(
        &self,
        sink: Option<Arc<crate::gpu_preview::GpuPreviewSink>>,
    ) -> Result<(), String> {
        if let Some(sink) = sink.as_ref()
            && sink.generation() != self.generation()
        {
            return Err("GPU preview source generation does not match.".into());
        }
        let mut slot = self
            .gpu
            .lock()
            .map_err(|_| "GPU preview hub was poisoned.")?;
        if let Some(old) = std::mem::replace(&mut *slot, sink) {
            old.close();
        }
        // A newly attached native viewer must receive the retained last frame,
        // even when a static desktop has produced no new media timestamp.
        self.subscription_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub fn gpu_sink(&self) -> Option<Arc<crate::gpu_preview::GpuPreviewSink>> {
        self.gpu.try_lock().ok()?.clone()
    }

    /// The CPU readback/PNG fallback is for idle preview. Live recording uses
    /// the independently paced GPU presenter; a slow frontend cannot invoke
    /// readback, PNG encoding or allocation on the recording capture thread.
    pub fn set_recording(&self) {
        self.recording.store(true, Ordering::Release);
    }

    pub(crate) fn cpu_preview_allowed(&self) -> bool {
        !self.recording.load(Ordering::Acquire)
    }

    pub(crate) fn subscription_epoch(&self) -> u64 {
        self.subscription_epoch.load(Ordering::Acquire)
    }

    /// True while the hub is open and at least one subscriber is attached.
    ///
    /// This is the single gate for all preview work: the capture worker asks it before
    /// every generation and performs no GPU or CPU preview work when it is false.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire) && self.subscriber_count() > 0
    }

    /// Attached subscribers.
    pub fn subscriber_count(&self) -> i64 {
        self.subscribers.load(Ordering::Acquire)
    }

    /// The worker-side counters of this hub.
    pub fn counters(&self) -> PreviewCounterSnapshot {
        self.counters.snapshot()
    }

    /// The counter handle itself, so a session can report its preview work beside the
    /// capture counters instead of maintaining a second copy.
    pub(crate) fn counters_handle(&self) -> &Arc<PreviewCounters> {
        &self.counters
    }

    /// Marks one completed surface for which no preview was attempted because nothing
    /// was subscribed. The worker calls this through the session so the diagnostic
    /// report can prove that preview work stopped.
    pub fn note_skipped(&self) {
        Counters::bump(&self.counters.skipped_no_subscriber);
    }

    /// Marks one completed surface skipped by the 10 fps bound.
    pub fn note_rate_limited(&self) {
        Counters::bump(&self.counters.rate_limited);
    }

    /// Registers one consumer.
    pub fn subscribe(self: &Arc<Self>) -> PreviewSubscriber {
        if self.subscribers.fetch_add(1, Ordering::AcqRel) == 0 {
            let previous_epoch = self.subscription_epoch.fetch_add(1, Ordering::AcqRel);
            if previous_epoch > 0 {
                // A producer may have finished a thumbnail after the last viewer
                // detached. Keep an initial image, but clear a prior viewer's image.
                let mut slot = lock(&self.slot);
                slot.image = None;
                slot.taken = false;
            }
        }
        self.ready.notify_all();
        PreviewSubscriber {
            hub: Arc::clone(self),
        }
    }

    /// Closes this hub permanently and wakes every waiter.
    ///
    /// Closing does not join or wait for the capture worker: the worker checks
    /// [`PreviewHub::enabled`] on its own thread and stops generating, so closing is
    /// bounded and can run on a UI thread.
    pub fn close(&self) {
        self.enabled.store(false, Ordering::Release);
        if let Ok(mut gpu) = self.gpu.lock()
            && let Some(sink) = gpu.take()
        {
            sink.close();
        }
        self.ready.notify_all();
    }

    /// The pending image, without taking it. Diagnostic use only.
    pub fn pending_image(&self) -> Option<PreviewImage> {
        lock(&self.slot).image.clone()
    }

    /// Takes the pending image, at most one unacknowledged image at a time.
    ///
    /// `None` means nothing new is available or the previous image has not been
    /// acknowledged yet. There is no queue: the producer replaces an untaken image.
    ///
    /// A pending image whose generation is not this hub's current one is dropped instead
    /// of handed out: after a source is replaced, the previous source's picture is never
    /// presented as the new source's.
    pub fn take(&self) -> Option<PreviewImage> {
        let mut slot = lock(&self.slot);
        if slot.taken {
            return None;
        }
        let current = slot.generation;
        let image = slot.image.take()?;
        if image.session_generation != current {
            return None;
        }
        slot.taken = true;
        Some(image)
    }

    /// Acknowledges the taken image, allowing the next one to be handed out.
    ///
    /// Acknowledging without an outstanding image is a no-op, so a double
    /// acknowledgement cannot corrupt the in-flight state.
    pub fn acknowledge(&self) {
        let mut slot = lock(&self.slot);
        slot.taken = false;
        drop(slot);
        self.ready.notify_all();
    }

    /// Blocks for at most `timeout` until something can be taken.
    ///
    /// - `Some(Ok(image))`: a fresh image; the caller must acknowledge it before taking
    ///   another one.
    /// - `Some(Err(()))`: the wait expired, or an image is still unacknowledged. This is
    ///   the empty case, never an error.
    /// - `None`: the hub was closed. The consumer must stop.
    pub fn wait_for_image(&self, timeout: Duration) -> Option<Result<PreviewImage, ()>> {
        if let Some(image) = self.take() {
            return Some(Ok(image));
        }
        if !self.enabled.load(Ordering::Acquire) {
            return None;
        }
        let deadline = Instant::now() + timeout;
        let mut slot = lock(&self.slot);
        loop {
            if !self.enabled.load(Ordering::Acquire) {
                return None;
            }
            if !slot.taken
                && let Some(image) = slot.image.take()
            {
                if image.session_generation != slot.generation {
                    // An image from a replaced source is dropped, never delivered as the
                    // current source's picture.
                    continue;
                }
                slot.taken = true;
                return Some(Ok(image));
            }
            let now = Instant::now();
            if now >= deadline {
                return Some(Err(()));
            }
            let (next, _) = self
                .ready
                .wait_timeout(slot, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poison| poison.into_inner());
            slot = next;
        }
    }

    /// Publishes one image, replacing an untaken predecessor.
    ///
    /// Returns the published sequence, or `None` when closed or the nonblocking
    /// slot lock is busy (counted as publication contention).
    /// It never blocks on a consumer and never grows a queue.
    ///
    /// This is the transport seam: a host that owns a preview source of its own (the
    /// desktop transport's own tests, a future non-DXGI producer) publishes through the
    /// same one-slot rule the capture worker uses, so the in-flight and drop policy can
    /// never diverge between producers.
    pub fn publish(&self, mut image: PreviewImage) -> Option<u64> {
        if !self.enabled.load(Ordering::Acquire) {
            return None;
        }
        // The cap is enforced here, at the one place an image can enter a hub, rather
        // than trusted from the producer.
        if let Ok(encoded) = &image.data
            && encoded.len() > MAX_PREVIEW_IMAGE_BYTES
        {
            image.data = Err(format!(
                "A {}-byte preview image exceeds the {MAX_PREVIEW_IMAGE_BYTES}-byte encoded-image cap, so no pixels are published.",
                encoded.len()
            ));
        }
        let mut slot = match self.slot.try_lock() {
            Ok(slot) => slot,
            Err(_) => {
                Counters::bump(&self.counters.publication_contention);
                return None;
            }
        };
        let sequence = slot.next_sequence;
        slot.next_sequence += 1;
        image.sequence = sequence;
        image.session_generation = slot.generation;
        image.first_of_session = slot.published_total == 0;
        slot.published_total += 1;
        if !slot.taken && slot.image.is_some() {
            Counters::bump(&self.counters.replaced_not_taken);
        }
        slot.image = Some(image);
        drop(slot);
        Counters::bump(&self.counters.published);
        self.ready.notify_all();
        Some(sequence)
    }

    /// Publishes one failure instead of pixels, for the UI error state.
    pub(crate) fn publish_error(&self, image: PreviewImage) {
        Counters::bump(&self.counters.failures);
        let _ = self.publish(image);
    }
}

/// Wraps a preview failure as an image the UI can display, so the worker never has to
/// turn a preview problem into a capture error.
pub(crate) fn error_image(
    width: u32,
    height: u32,
    source_media_ns: Option<u64>,
    message: String,
) -> PreviewImage {
    PreviewImage {
        width,
        height,
        sequence: 0,
        first_of_session: false,
        source_media_ns,
        session_generation: 0,
        mime_type: PREVIEW_IMAGE_MIME,
        data: Err(message),
    }
}

/// One attached consumer. Dropping it detaches the subscriber, after which the capture
/// worker stops producing images.
pub struct PreviewSubscriber {
    hub: Arc<PreviewHub>,
}

impl PreviewSubscriber {
    /// The hub this subscriber is attached to.
    pub fn hub(&self) -> &Arc<PreviewHub> {
        &self.hub
    }
}

impl Drop for PreviewSubscriber {
    fn drop(&mut self) {
        let previous = self.hub.subscribers.fetch_sub(1, Ordering::AcqRel);
        if previous == 1 {
            // A later subscriber must not receive the last viewer's image. This slot
            // contains encoded bytes only; it holds no capture surface lease.
            let mut slot = lock(&self.hub.slot);
            slot.image = None;
            slot.taken = false;
        }
        self.hub.ready.notify_all();
    }
}

/// Locks a hub mutex, treating poison as recoverable: the slot holds plain data and no
/// invariant can be broken by a panic in a consumer.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Checks the complete session payload before any preview texture is allocated.
///
/// The direct path owns only a staging texture. The reduced path owns a full-source
/// intermediate plus preview-sized render and staging textures. The two raw CPU
/// images admitted for the encoder (one processing and one replaceable pending) also
/// count against the session payload budget. During a geometry change both GPU
/// stages count until the replacement is ready.
pub(crate) fn preview_payload_plan(
    config: PreviewConfig,
    source_width: u32,
    source_height: u32,
    capture_bytes: u64,
    previous_stage_bytes: u64,
) -> Result<u64, RecorderError> {
    config.validate()?;
    let geometry = preview_geometry(config, source_width, source_height)
        .map_err(|error| internal(error.message()))?;
    let texture_bytes = |width: u32, height: u32| {
        u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
    };
    let image_bytes = texture_bytes(geometry.width, geometry.height)
        .ok_or_else(|| internal("The preview image payload size overflowed."))?;
    let stage_bytes = if geometry.source_box.is_none() {
        image_bytes
    } else {
        texture_bytes(source_width, source_height)
            .and_then(|source| {
                image_bytes
                    .checked_mul(2)
                    .and_then(|pair| source.checked_add(pair))
            })
            .ok_or_else(|| internal("The preview texture payload size overflowed."))?
    };
    let cpu_queue_bytes = image_bytes
        .checked_mul(2)
        .ok_or_else(|| internal("The preview CPU queue payload size overflowed."))?;
    let combined = capture_bytes
        .checked_add(previous_stage_bytes)
        .and_then(|bytes| bytes.checked_add(stage_bytes))
        .and_then(|bytes| bytes.checked_add(cpu_queue_bytes))
        .ok_or_else(|| internal("The combined capture and preview payload size overflowed."))?;
    if combined > MAX_PAYLOAD_BYTES {
        return Err(internal(format!(
            "The {source_width}x{source_height} preview needs {stage_bytes} GPU bytes and {cpu_queue_bytes} CPU queue bytes (two {}x{} raw images); capture uses {capture_bytes} bytes and the previous preview stage still holds {previous_stage_bytes} bytes. Their {combined}-byte peak exceeds the {MAX_PAYLOAD_BYTES}-byte session payload budget before allocation.",
            geometry.width, geometry.height,
        )));
    }
    Ok(stage_bytes)
}

/// A source rectangle, so the geometry stays a plain comparable value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewBox {
    pub(crate) left: u32,
    pub(crate) top: u32,
    pub(crate) right: u32,
    pub(crate) bottom: u32,
}

/// Which source rectangle is reduced into which preview image, as pure arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewGeometry {
    /// The capture surface this image is produced from.
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    /// Pixels actually read back: the image size.
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// `None` when the source is not larger than the image and is copied 1:1.
    pub(crate) source_box: Option<PreviewBox>,
}

/// Decides the source rectangle and the image size for one capture surface.
///
/// Fit the whole source within the requested bounds with one aspect-preserving scale.
/// The non-limiting dimension rounds down by less than one pixel. D3D11's video
/// processor supports fractional reduction; cropping to an integer ratio would lose
/// edge pixels and unnecessarily reduce detail. Sources that already fit stay 1:1.
pub(crate) fn preview_geometry(
    config: PreviewConfig,
    source_width: u32,
    source_height: u32,
) -> Result<PreviewGeometry, PreviewError> {
    if source_width == 0 || source_height == 0 {
        return Err(PreviewError::Unsupported(format!(
            "The capture surface reports {source_width}x{source_height}, which has no pixels to preview."
        )));
    }
    if config.native && (source_width > config.max_width || source_height > config.max_height) {
        return Err(PreviewError::Unsupported(format!(
            "Native preview of {source_width}x{source_height} exceeds the explicit {}x{} source limit. Choose a reduced preview in Settings; recording resolution is unchanged.",
            config.max_width, config.max_height
        )));
    }
    if source_width <= config.max_width && source_height <= config.max_height {
        return Ok(PreviewGeometry {
            source_width,
            source_height,
            width: source_width,
            height: source_height,
            source_box: None,
        });
    }
    let (width, height) = if u64::from(source_width) * u64::from(config.max_height)
        >= u64::from(source_height) * u64::from(config.max_width)
    {
        (
            config.max_width,
            (u64::from(source_height) * u64::from(config.max_width) / u64::from(source_width))
                as u32,
        )
    } else {
        (
            (u64::from(source_width) * u64::from(config.max_height) / u64::from(source_height))
                as u32,
            config.max_height,
        )
    };
    if width == 0 || height == 0 {
        return Err(PreviewError::Unsupported(
            "The source aspect ratio is too extreme for the requested preview size.".to_string(),
        ));
    }
    Ok(PreviewGeometry {
        source_width,
        source_height,
        width,
        height,
        source_box: Some(PreviewBox {
            left: 0,
            top: 0,
            right: source_width,
            bottom: source_height,
        }),
    })
}

/// A preview preparation or readback failure, classified before it reaches the UI.
#[derive(Clone, Debug)]
pub(crate) enum PreviewError {
    /// The source cannot be reduced into the configured bound, or this adapter cannot
    /// reduce it on the GPU.
    Unsupported(String),
    /// A native or resource failure.
    Failed(RecorderError),
}

impl PreviewError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Unsupported(message) => message.clone(),
            Self::Failed(error) => error.message.clone(),
        }
    }
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for PreviewError {}

/// The frame rate the video processor is configured for. The preview itself is gated to
/// [`PREVIEW_MIN_INTERVAL`], so this is only a declaration of the content's cadence.
const PREVIEW_MAX_FPS_NUM: u32 = 10;

/// One capture texture a preview image can be produced from.
///
/// It holds a **typed, owned** reference to the session's own texture: the handle comes
/// from the capture payload that keeps the texture alive, `Clone` takes the documented COM
/// reference count and `Drop` releases exactly that reference. The raw interface pointer
/// never leaves this type, and nothing here reinterprets the COM object as wrapper memory
/// or releases a reference it does not own.
pub struct PreviewSource {
    pub(crate) texture: ID3D11Texture2D,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl PreviewSource {
    /// The image size and texture this preview would be produced from new.
    ///
    /// The caller passes the typed handle it owns; this takes its own reference to it.
    pub(crate) fn new(texture: &ID3D11Texture2D, width: u32, height: u32) -> Self {
        Self {
            texture: texture.clone(),
            width,
            height,
        }
    }
}

/// The bounded D3D11 preview readback.
///
/// It is created and used only on the capture worker thread that owns the device and its
/// immediate context. It holds no duplicate of the capture payload: the source texture
/// is passed in per generation and stays owned by the session.
pub(crate) struct ThumbnailGenerator {
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    /// Capture slots and optional verification staging, already admitted at startup.
    capture_payload_bytes: u64,
    /// The video processor context of the same immediate context, created with the stage.
    video_context: Option<ID3D11VideoContext>,
    config: PreviewConfig,
    /// The reduction resources, recreated only when the geometry changes.
    stage: Option<PreviewStage>,
    /// The CPU-readable copy of the small image.
    staging: Option<ID3D11Texture2D>,
    staging_size: (u32, u32),
    /// The one image queued for a map, with the surface it came from.
    pending: Option<PendingPreview>,
    counters: Arc<PreviewCounters>,
}

/// The GPU reduction resources one geometry needs: the Frame-owned 1:1 intermediate, the
/// small render target the video processor scales into, and the two video processor views.
struct PreviewStage {
    intermediate: ID3D11Texture2D,
    source_size: (u32, u32),
    render_target: ID3D11Texture2D,
    image_size: (u32, u32),
    payload_bytes: u64,
    /// Kept alive for as long as the processor and the two views created from it: it is
    /// the parent object of this reduction, so it is a lifetime anchor rather than a value
    /// this code reads back.
    #[allow(dead_code)]
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
}

/// One reduction that is queued on the GPU and waiting for a single non-blocking map.
#[derive(Clone, Copy, Debug)]
struct PendingPreview {
    geometry: PreviewGeometry,
    source_media_ns: Option<u64>,
}

impl std::fmt::Debug for ThumbnailGenerator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThumbnailGenerator")
            .field("config", &self.config)
            .field("has_device", &self.device.is_some())
            .field("has_context", &self.context.is_some())
            .field(
                "source_size",
                &self.stage.as_ref().map(|stage| stage.source_size),
            )
            .field(
                "image_size",
                &self.stage.as_ref().map(|stage| stage.image_size),
            )
            .field("staging", &self.staging_size)
            .finish_non_exhaustive()
    }
}

impl ThumbnailGenerator {
    /// Creates the generator for one session's device and preview request.
    ///
    /// The request is validated first, so an out-of-bound preview is refused before the
    /// device or any texture is touched.
    pub(crate) fn create(
        device: Option<ID3D11Device>,
        context: Option<ID3D11DeviceContext>,
        config: PreviewConfig,
        capture_payload_bytes: u64,
        counters: &Arc<PreviewCounters>,
    ) -> Result<Self, PreviewError> {
        config.validate().map_err(PreviewError::Failed)?;
        Ok(Self {
            device,
            context,
            capture_payload_bytes,
            video_context: None,
            config,
            stage: None,
            staging: None,
            staging_size: (0, 0),
            pending: None,
            counters: Arc::clone(counters),
        })
    }

    /// The request this generator enforces.
    pub(crate) fn config(&self) -> PreviewConfig {
        self.config
    }

    /// Queues one GPU reduction for a newly completed source surface.
    ///
    /// A previous staging readback owns the one pending slot until it completes or is
    /// discarded. It is never overwritten, and this call does not map or encode.
    pub(crate) fn queue(
        &mut self,
        source: PreviewSource,
        source_media_ns: Option<u64>,
    ) -> Result<bool, PreviewError> {
        if self.pending.is_some() {
            return Ok(false);
        }
        let geometry = preview_geometry(self.config, source.width, source.height)?;
        // The handles are cloned, so the generator's own fields stay free to be borrowed
        // mutably by the two stages below.
        let (Some(device), Some(context)) = (self.device.clone(), self.context.clone()) else {
            return Err(PreviewError::Failed(internal(
                "This preview generator has no capture device or immediate context, so it cannot reduce a desktop surface.",
            )));
        };

        self.queue_reduction(&device, &context, &source, &geometry)?;
        self.pending = Some(PendingPreview {
            geometry,
            source_media_ns,
        });

        Ok(true)
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn discard_pending(&mut self) {
        self.pending = None;
    }

    /// Tries one `DO_NOT_WAIT` map on the worker's next bounded acquisition step.
    /// A busy staging texture stays pending for a later step, including when the
    /// desktop remains static and no further source surface arrives.
    pub(crate) fn complete_pending(
        &mut self,
        output: &mut Vec<u8>,
    ) -> Result<Option<(PreviewImage, PreviewCost)>, PreviewError> {
        if self.pending.is_none() {
            return Ok(None);
        }
        let Some(context) = self.context.clone() else {
            return Err(PreviewError::Failed(internal(
                "This preview generator has no immediate context for its pending readback.",
            )));
        };
        let readback_started = Instant::now();
        let finished = self.finish_pending(&context, output)?;
        let readback_nanos = nanos(readback_started.elapsed());
        self.counters
            .readback_nanos
            .fetch_add(readback_nanos, Ordering::Relaxed);
        let Some((width, height, media_ns)) = finished else {
            return Ok(None);
        };
        let encode_started = Instant::now();
        let mut encoded = Vec::with_capacity(96 * 1024);
        encode_png(output, width, height, &mut encoded).map_err(PreviewError::Failed)?;
        let encode_nanos = nanos(encode_started.elapsed());
        self.counters
            .encode_nanos
            .fetch_add(encode_nanos, Ordering::Relaxed);
        Counters::bump(&self.counters.generated);

        let cost = PreviewCost {
            readback_nanos,
            encode_nanos,
            image_width: width,
            image_height: height,
            encoded_bytes: encoded.len(),
        };
        Ok(Some((
            PreviewImage {
                width,
                height,
                sequence: 0,
                first_of_session: false,
                source_media_ns: media_ns,
                session_generation: 0,
                mime_type: PREVIEW_IMAGE_MIME,
                data: Ok(encoded),
            },
            cost,
        )))
    }

    /// Completes only the GPU readback. The capture worker passes the resulting
    /// owned BGRA image to its bounded CPU encoder thread.
    #[allow(clippy::type_complexity)]
    pub(crate) fn readback_pending(
        &mut self,
        output: &mut Vec<u8>,
    ) -> Result<Option<(u32, u32, Option<u64>, u64)>, PreviewError> {
        if self.pending.is_none() {
            return Ok(None);
        }
        let Some(context) = self.context.clone() else {
            return Err(PreviewError::Failed(internal(
                "This preview generator has no immediate context for its pending readback.",
            )));
        };
        let started = Instant::now();
        let finished = self.finish_pending(&context, output)?;
        let readback_nanos = nanos(started.elapsed());
        self.counters
            .readback_nanos
            .fetch_add(readback_nanos, Ordering::Relaxed);
        Ok(finished.map(|(width, height, media_ns)| (width, height, media_ns, readback_nanos)))
    }

    /// One non-blocking `Map` of the staging texture the last queued reduction wrote.
    ///
    /// A busy resource remains pending, never waited for. `Ok(None)` means "retry on
    /// a later worker step". On success the pixels are written into the caller's
    /// reused buffer, so no per-image allocation happens here.
    fn finish_pending(
        &mut self,
        context: &ID3D11DeviceContext,
        output: &mut Vec<u8>,
    ) -> Result<Option<(u32, u32, Option<u64>)>, PreviewError> {
        let Some(pending) = self.pending else {
            return Ok(None);
        };
        let Some(staging) = self.staging.as_ref() else {
            return Ok(None);
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the map targets this device's own staging texture, subresource 0 is valid
        // for a single-mip 2D texture, and DO_NOT_WAIT keeps the call from blocking inside
        // the driver, so a busy resource is reported instead of waited for.
        let result = unsafe {
            context.Map(
                staging,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        };
        match result {
            Ok(()) => {
                // From here exactly one Unmap follows, including on the error path.
                let outcome = copy_mapped(
                    &mapped,
                    pending.geometry.width,
                    pending.geometry.height,
                    output,
                );
                // SAFETY: balances the successful Map above, exactly once, on the worker
                // thread that owns the context.
                unsafe { context.Unmap(staging, 0) };
                self.pending = None;
                outcome?;
                Ok(Some((
                    pending.geometry.width,
                    pending.geometry.height,
                    pending.source_media_ns,
                )))
            }
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                Counters::bump(&self.counters.map_busy);
                Ok(None)
            }
            Err(error) => Err(PreviewError::Failed(map_native_error(
                "ID3D11DeviceContext::Map(preview staging)",
                error,
            ))),
        }
    }

    /// Copies a native capture texture directly to staging, or reduces it on the GPU.
    ///
    /// `CopySubresourceRegion` never scales, so the reduction is a real GPU scale: the
    /// session's slot is copied 1:1 into a Frame-owned intermediate (`CopyResource`), and
    /// that intermediate is then scaled by the D3D11 video processor
    /// (`VideoProcessorBlt`) from the exact source box into a small render target. Only
    /// the resulting image is copied into the staging texture, so the CPU readback is
    /// bounded by the image size and never by the display resolution.
    fn queue_reduction(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &PreviewSource,
        geometry: &PreviewGeometry,
    ) -> Result<(), PreviewError> {
        if geometry.source_box.is_none() {
            self.ensure_direct_staging(device, geometry)?;
            let staging = self.staging.as_ref().ok_or_else(|| {
                PreviewError::Failed(internal("The preview staging texture was not created."))
            })?;
            // SAFETY: source and staging have identical dimensions and BGRA8 format.
            // Both belong to this device and the worker owns its immediate context.
            unsafe {
                context.CopyResource(staging, &source.texture);
                context.Flush();
            }
            return Ok(());
        }
        self.ensure_stage(device, context, geometry)?;
        let Some(stage) = self.stage.as_ref() else {
            return Err(PreviewError::Failed(internal(
                "The preview reduction resources were not created.",
            )));
        };
        if stage.image_size != (geometry.width, geometry.height)
            || stage.source_size != (geometry.source_width, geometry.source_height)
        {
            return Err(PreviewError::Failed(internal(
                "The preview reduction resources do not match this geometry.",
            )));
        }
        let Some(staging) = self.staging.as_ref() else {
            return Err(PreviewError::Failed(internal(
                "The preview staging texture was not created.",
            )));
        };
        let Some(video_context) = self.video_context.as_ref() else {
            return Err(PreviewError::Failed(internal(
                "The preview video processor context was not created.",
            )));
        };
        let box_ = geometry.source_box.expect("the direct path returned above");
        let source_rect = RECT {
            left: i32::try_from(box_.left).unwrap_or(i32::MAX),
            top: i32::try_from(box_.top).unwrap_or(i32::MAX),
            right: i32::try_from(box_.right).unwrap_or(i32::MAX),
            bottom: i32::try_from(box_.bottom).unwrap_or(i32::MAX),
        };
        let target_rect = RECT {
            left: 0,
            top: 0,
            right: i32::try_from(geometry.width).unwrap_or(i32::MAX),
            bottom: i32::try_from(geometry.height).unwrap_or(i32::MAX),
        };
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(stage.input_view.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        // SAFETY: every call targets this device's own resources on the worker thread that
        // owns the immediate context. The 1:1 copy is the documented `CopyResource` case
        // (same size and format); the source box lies inside the intermediate by
        // construction; the video processor scales that box into a render target of exactly
        // the image size; and the final copy moves one whole subresource into a staging
        // texture of exactly the same size, which is the case `CopySubresourceRegion`
        // supports. `Flush` only submits this worker's own queued commands.
        unsafe {
            context.CopyResource(&stage.intermediate, &source.texture);
            video_context.VideoProcessorSetStreamFrameFormat(
                &stage.processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamSourceRect(
                &stage.processor,
                0,
                true,
                Some(&source_rect),
            );
            video_context.VideoProcessorSetStreamDestRect(
                &stage.processor,
                0,
                true,
                Some(&target_rect),
            );
            video_context.VideoProcessorSetOutputTargetRect(
                &stage.processor,
                true,
                Some(&target_rect),
            );
            video_context
                .VideoProcessorBlt(&stage.processor, &stage.output_view, 0, &[stream])
                .map_err(|error| {
                    PreviewError::Failed(map_native_error(
                        "ID3D11VideoContext::VideoProcessorBlt(preview)",
                        error,
                    ))
                })?;
            context.CopySubresourceRegion(staging, 0, 0, 0, 0, &stage.render_target, 0, None);
            context.Flush();
        }
        Ok(())
    }

    /// Allocates only the one CPU-readable texture required for a 1:1 preview.
    fn ensure_direct_staging(
        &mut self,
        device: &ID3D11Device,
        geometry: &PreviewGeometry,
    ) -> Result<(), PreviewError> {
        let image_size = (geometry.width, geometry.height);
        if self.staging_size != image_size || self.staging.is_none() {
            let previous_stage_bytes = self.stage.as_ref().map_or_else(
                || u64::from(self.staging_size.0) * u64::from(self.staging_size.1) * 4,
                |stage| stage.payload_bytes,
            );
            preview_payload_plan(
                self.config,
                geometry.source_width,
                geometry.source_height,
                self.capture_payload_bytes,
                previous_stage_bytes,
            )
            .map_err(PreviewError::Failed)?;
            self.staging = Some(create_texture(
                device,
                image_size.0,
                image_size.1,
                TextureRole::Staging,
            )?);
            self.staging_size = image_size;
        }
        self.stage = None;
        self.video_context = None;
        Ok(())
    }

    /// Creates the reduction resources for one geometry, once.
    ///
    /// The capture slot is created with no bind flags of its own, so it can never carry a
    /// video processor view: the 1:1 `CopyResource` into the Frame-owned intermediate is
    /// what makes the reduction a scale of a texture this module owns. The intermediate and
    /// the small render target are created with the bind flags the video processor needs,
    /// and both live only as long as this geometry does.
    fn ensure_stage(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        geometry: &PreviewGeometry,
    ) -> Result<(), PreviewError> {
        let source_size = (geometry.source_width, geometry.source_height);
        let image_size = (geometry.width, geometry.height);
        let usable = self.staging_size == image_size
            && self.staging.is_some()
            && self.stage.as_ref().is_some_and(|stage| {
                stage.source_size == source_size && stage.image_size == image_size
            });
        if usable {
            return Ok(());
        }
        if geometry.width > self.config.max_width || geometry.height > self.config.max_height {
            return Err(PreviewError::Unsupported(format!(
                "A {}x{} preview image exceeds the {}x{} bound.",
                geometry.width, geometry.height, self.config.max_width, self.config.max_height
            )));
        }
        let previous_stage_bytes = self.stage.as_ref().map_or(0, |stage| stage.payload_bytes);
        let payload_bytes = preview_payload_plan(
            self.config,
            geometry.source_width,
            geometry.source_height,
            self.capture_payload_bytes,
            previous_stage_bytes,
        )
        .map_err(PreviewError::Failed)?;
        let intermediate =
            create_texture(device, source_size.0, source_size.1, TextureRole::Reduce)?;
        let render_target = create_texture(
            device,
            image_size.0,
            image_size.1,
            TextureRole::RenderTarget,
        )?;
        let staging = create_texture(device, image_size.0, image_size.1, TextureRole::Staging)?;
        let video_device: ID3D11VideoDevice = device.cast().map_err(|error| {
            PreviewError::Unsupported(native_message(
                "ID3D11Device::cast<ID3D11VideoDevice>",
                &error,
            ))
        })?;
        let video_context: ID3D11VideoContext = context.cast().map_err(|error| {
            PreviewError::Unsupported(native_message(
                "ID3D11DeviceContext::cast<ID3D11VideoContext>",
                &error,
            ))
        })?;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: PREVIEW_MAX_FPS_NUM,
                Denominator: 1,
            },
            InputWidth: source_size.0,
            InputHeight: source_size.1,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: PREVIEW_MAX_FPS_NUM,
                Denominator: 1,
            },
            OutputWidth: image_size.0,
            OutputHeight: image_size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: every call creates one Frame-owned resource from a fully initialized
        // descriptor on this session's own device, and every out-parameter is written only
        // on success.
        let (enumerator, processor, input_view, output_view) = unsafe {
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content)
                .map_err(|error| {
                    PreviewError::Unsupported(native_message(
                        "ID3D11VideoDevice::CreateVideoProcessorEnumerator(preview)",
                        &error,
                    ))
                })?;
            let support = enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(|error| {
                    PreviewError::Unsupported(native_message(
                        "ID3D11VideoProcessorEnumerator::CheckVideoProcessorFormat(preview)",
                        &error,
                    ))
                })?;
            if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
                || support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
            {
                return Err(PreviewError::Unsupported(
                    "This video processor cannot reduce a BGRA8 surface, so the preview readback is unavailable on this adapter."
                        .to_string(),
                ));
            }
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|error| {
                    PreviewError::Unsupported(native_message(
                        "ID3D11VideoDevice::CreateVideoProcessor(preview)",
                        &error,
                    ))
                })?;
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            video_device
                .CreateVideoProcessorInputView(
                    &intermediate,
                    &enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )
                .map_err(|error| {
                    PreviewError::Unsupported(native_message(
                        "ID3D11VideoDevice::CreateVideoProcessorInputView(preview)",
                        &error,
                    ))
                })?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device
                .CreateVideoProcessorOutputView(
                    &render_target,
                    &enumerator,
                    &output_desc,
                    Some(&mut output_view),
                )
                .map_err(|error| {
                    PreviewError::Unsupported(native_message(
                        "ID3D11VideoDevice::CreateVideoProcessorOutputView(preview)",
                        &error,
                    ))
                })?;
            let (Some(input_view), Some(output_view)) = (input_view, output_view) else {
                return Err(PreviewError::Failed(internal(
                    "A video processor view reported success without a view.",
                )));
            };
            (enumerator, processor, input_view, output_view)
        };
        self.stage = Some(PreviewStage {
            intermediate,
            source_size,
            render_target,
            image_size,
            payload_bytes,
            enumerator,
            processor,
            input_view,
            output_view,
        });
        self.video_context = Some(video_context);
        self.staging = Some(staging);
        self.staging_size = image_size;
        self.pending = None;
        Ok(())
    }
}

/// Which kind of preview texture is being created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextureRole {
    /// The Frame-owned 1:1 copy of a capture slot, usable as a video processor input.
    Reduce,
    /// The small image the video processor scales into.
    RenderTarget,
    /// The small CPU-readable copy of that image.
    Staging,
}

/// Creates one BGRA8 preview texture with the bind flags its role needs.
fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    role: TextureRole,
) -> Result<ID3D11Texture2D, PreviewError> {
    let (usage, bind, cpu, misc) = match role {
        // A copy target that must also be a video processor input and a blit target: the
        // reduction is a scale of this texture, never of the capture slot.
        TextureRole::Reduce => (
            D3D11_USAGE_DEFAULT,
            (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
            0,
            0,
        ),
        // The video processor writes this one, so it must be a render target.
        TextureRole::RenderTarget => (
            D3D11_USAGE_DEFAULT,
            (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            0,
            0,
        ),
        // The only CPU-readable resource on this path, and it holds the small image.
        TextureRole::Staging => (D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32, 0),
    };
    let descriptor = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: misc,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: the descriptor is fully initialized and no initial data is passed, so the API
    // only writes into the out-parameter. The texture belongs to the session's own capture
    // device and is used only on its worker thread.
    unsafe {
        device
            .CreateTexture2D(&descriptor, None, Some(&mut texture))
            .map_err(|error| {
                PreviewError::Failed(map_native_error(
                    "ID3D11Device::CreateTexture2D(preview)",
                    error,
                ))
            })?;
    }
    texture.ok_or_else(|| {
        PreviewError::Failed(internal(
            "CreateTexture2D reported success without a preview texture.",
        ))
    })
}

/// The HRESULT text of a native call, for a refusal that names what the adapter lacks.
fn native_message(api: &str, error: &windows::core::Error) -> String {
    format!(
        "{api} failed with HRESULT 0x{:08X}, so this adapter cannot reduce a preview image on the GPU.",
        error.code().0
    )
}
/// Copies one mapped staging image into `pixels`, excluding row padding.
fn copy_mapped(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
    pixels: &mut Vec<u8>,
) -> Result<(), PreviewError> {
    if mapped.pData.is_null() {
        return Err(PreviewError::Failed(internal(
            "The preview staging texture mapped to a null pointer.",
        )));
    }
    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| PreviewError::Failed(internal("The preview row width overflowed.")))?;
    let row_pitch = mapped.RowPitch as usize;
    if row_pitch < row_bytes {
        return Err(PreviewError::Failed(internal(format!(
            "The mapped preview row pitch {row_pitch} is smaller than the {row_bytes} valid BGRA bytes of one row."
        ))));
    }
    let height = height as usize;
    let mapped_bytes = row_pitch
        .checked_mul(height)
        .ok_or_else(|| PreviewError::Failed(internal("The mapped preview size overflowed.")))?;
    let span = row_pitch
        .checked_mul(height.saturating_sub(1))
        .and_then(|rows| rows.checked_add(row_bytes))
        .ok_or_else(|| PreviewError::Failed(internal("The mapped preview span overflowed.")))?;
    if span > isize::MAX as usize {
        return Err(PreviewError::Failed(internal(
            "The mapped preview span exceeds Rust's pointer offset limit.",
        )));
    }
    if mapped_bytes < span {
        return Err(PreviewError::Failed(internal(
            "The mapped preview allocation is smaller than the image rows it reports.",
        )));
    }
    let total = row_bytes
        .checked_mul(height)
        .ok_or_else(|| PreviewError::Failed(internal("The preview image size overflowed.")))?;
    pixels.clear();
    pixels.reserve(total);
    for row in 0..height {
        let start = row * row_pitch;
        // SAFETY: the checks above proved that every row offset plus the valid row
        // bytes stays inside the span D3D11 reported for this mapping and that the span
        // is a valid allocation. The pixel bytes were written by the GPU copy whose
        // completion preceded this successful map; row padding is never read.
        let row_slice =
            unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(start), row_bytes) };
        pixels.extend_from_slice(row_slice);
    }
    Ok(())
}

// ------------------------------------------------------------------ PNG encoding

/// Encodes one BGRA8 capture image as opaque RGB PNG. Capture alpha is not meaningful;
/// discarding it avoids a transparent or dark preview and reduces IPC payload size.
pub(crate) fn encode_png(
    bgra: &[u8],
    width: u32,
    height: u32,
    output: &mut Vec<u8>,
) -> Result<(), RecorderError> {
    if width == 0 || height == 0 || width > PREVIEW_MAX_WIDTH || height > PREVIEW_MAX_HEIGHT {
        return Err(internal(format!(
            "A {width}x{height} preview image must fit within 1x1 and {PREVIEW_MAX_WIDTH}x{PREVIEW_MAX_HEIGHT}."
        )));
    }
    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| internal("The preview image row width overflowed."))?;
    let expected = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| internal("The preview image size overflowed."))?;
    if bgra.len() != expected {
        return Err(internal(format!(
            "The preview image expects {expected} BGRA bytes for {width}x{height} but {} were supplied.",
            bgra.len()
        )));
    }
    output.clear();
    // Signature, then IHDR with color type 2 (truecolor), 8 bits per
    // channel and no interlacing.
    output.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut header = [0u8; 13];
    header[0..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8] = 8;
    header[9] = 2;
    write_chunk(output, b"IHDR", &header);

    // Encode each row directly into zlib. BGRA is a DXGI storage format; PNG expects
    // RGB order, and the source alpha must not make desktop pixels transparent.
    let mut deflate = ZlibEncoder::new(Vec::new(), Compression::fast());
    // Four spare bytes permit the final 16-byte SIMD store of twelve RGB bytes.
    let mut rgb_row = vec![0u8; width as usize * 3 + 5];
    let png_row_bytes = width as usize * 3 + 1;
    for row in 0..height as usize {
        rgb_row[0] = 0; // PNG filter None.
        let source_row = &bgra[row * row_bytes..(row + 1) * row_bytes];
        bgra_to_rgb_row(source_row, &mut rgb_row[1..]);
        deflate
            .write_all(&rgb_row[..png_row_bytes])
            .map_err(|error| internal(format!("The preview PNG compressor failed: {error}")))?;
    }
    let deflate = deflate
        .finish()
        .map_err(|error| internal(format!("The preview PNG compressor failed: {error}")))?;
    write_chunk(output, b"IDAT", &deflate);
    write_chunk(output, b"IEND", &[]);
    if output.len() > MAX_PREVIEW_IMAGE_BYTES {
        return Err(internal(format!(
            "The encoded preview image is {} bytes, past the {MAX_PREVIEW_IMAGE_BYTES}-byte cap.",
            output.len()
        )));
    }
    Ok(())
}

/// Swizzles one BGRA scanline into the exact RGB bytes required by PNG.
fn bgra_to_rgb_row(source: &[u8], rgb: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("ssse3") {
        // SAFETY: the feature is checked, source has four bytes per pixel, and the
        // caller provides four spare target bytes for the final overlapping store.
        unsafe { bgra_to_rgb_row_ssse3(source, rgb) };
        return;
    }
    for (source, target) in source.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
        target.copy_from_slice(&[source[2], source[1], source[0]]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn bgra_to_rgb_row_ssse3(source: &[u8], rgb: &mut [u8]) {
    use std::arch::x86_64::{
        __m128i, _mm_loadu_si128, _mm_setr_epi8, _mm_shuffle_epi8, _mm_storeu_si128,
    };

    let mask = _mm_setr_epi8(2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -1, -1, -1, -1);
    let mut pixel = 0;
    let pixels = source.len() / 4;
    while pixel + 4 <= pixels {
        // SAFETY: four pixels require 16 source bytes. The target has 3*width+4
        // bytes, so a 16-byte store at 3*pixel remains in bounds, including the
        // final block. Each subsequent block overwrites the prior four spare bytes.
        unsafe {
            let input = _mm_loadu_si128(source.as_ptr().add(pixel * 4).cast::<__m128i>());
            let output = _mm_shuffle_epi8(input, mask);
            _mm_storeu_si128(rgb.as_mut_ptr().add(pixel * 3).cast::<__m128i>(), output);
        }
        pixel += 4;
    }
    for index in pixel..pixels {
        rgb[index * 3..index * 3 + 3].copy_from_slice(&[
            source[index * 4 + 2],
            source[index * 4 + 1],
            source[index * 4],
        ]);
    }
}

/// Writes one PNG chunk: length, type, data and the CRC32 over type and data.
fn write_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    let length = data.len() as u32;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    output.extend_from_slice(&crc.finish().to_be_bytes());
}

/// The CRC-32 (IEEE 802.3) that every PNG chunk carries.
/// Build the 1 KiB lookup table once at compile time: native previews must not
/// perform eight polynomial steps for every payload byte on the capture worker.
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = (value >> 1) ^ if value & 1 == 1 { 0xedb8_8320 } else { 0 };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
};

struct Crc32 {
    state: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { state: !0 }
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            let index = ((self.state ^ u32::from(*byte)) & 0xff) as usize;
            self.state = (self.state >> 8) ^ CRC32_TABLE[index];
        }
    }

    fn finish(&self) -> u32 {
        !self.state
    }
}

// ------------------------------------------------------------------------- errors

fn invalid_request(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidRequest,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: format!(
            "Request at most {PREVIEW_MAX_WIDTH}x{PREVIEW_MAX_HEIGHT} at 10 fps for a preview."
        ),
    }
}

fn internal(message: impl Into<String>) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry the preview; if it persists, report the Windows build, GPU and driver."
            .to_string(),
    }
}

fn map_native_error(api: &str, error: windows::core::Error) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "{api} failed with HRESULT 0x{:08X} during a preview readback.",
            error.code().0
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "The preview image was skipped; the recording is unaffected. Retry or close the preview."
            .to_string(),
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
