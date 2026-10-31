//! GPU region (crop) capture for one selected display (FRAME-0205).
//!
//! One [`RegionCaptureSession`] wraps an accepted DXGI display capture session and copies
//! one canonical rectangle out of every completed surface into a bounded set of crop
//! textures **on the same Direct3D 11 device**, then hands the copies out as ordinary
//! [`WindowsSurface`] leases. Nothing is read back to the CPU: the copy is
//! `ID3D11DeviceContext::CopySubresourceRegion` with a `D3D11_BOX`, and each copy's
//! completion is observed through its own `D3D11_QUERY_EVENT` before the lease exists.
//!
//! Canonical region coordinates (frame-settings) are the selected display's own physical
//! pixel space, with the origin at that display's top-left corner and a half-open
//! rectangle. `D3D11_BOX` is inclusive at `left`/`top` and exclusive at `right`/`bottom`,
//! which is exactly that rectangle, so no edge is moved by one pixel.
//!
//! What this module deliberately does not do: it does not encode, mux, record, render,
//! rescale, clamp or read back pixels, and it adds no thread, queue, timer or channel. The
//! display capture algorithm is untouched — the session is opened through the accepted
//! public capture backend and its surfaces are consumed through the accepted
//! `CaptureSession` contract.
//!
//! Ownership rules
//! ---------------
//!
//! - **Lease:** one crop lease owns one slot of the crop pool *and* the capture lease the
//!   rectangle was copied from, so the capture slot cannot be recycled while a consumer
//!   still holds the crop. That is why the crop pool cannot be exhausted by a consumer that
//!   follows the accepted contract: a display surface exists only while every crop slot the
//!   consumer could still be holding is free, so pressure is resolved where the accepted
//!   design already resolves it — by the capture session's bounded drop policy. The crop
//!   pool never grows.
//! - **Backpressure:** the display session's bounded ready queue is the pressure point;
//!   this session never waits for a new surface (an empty read is `Ok(None)`) and never
//!   allocates a texture because a consumer stalled. A copy that cannot start because every
//!   crop slot is still held is an explicit error with its own counter, never a silent
//!   drop, and the copy's completion is bounded by a watchdog instead of an unbounded wait.
//! - **Stop:** `stop` stops the display session (its worker and its own payload) and drops
//!   this session's crop engine — its device reference, crop textures and queries.
//!   Outstanding crop leases stay valid: they hold their own reference to the crop payload
//!   and, through it, to the capture device. Capture and crop GPU memory are preflighted
//!   and reported separately by [`RegionCaptureSession::diagnostics`].
//!
//! Bounds: at most four crop textures (the capture slot count), one completion query per
//! slot, no CPU copy anywhere, and a five-second copy-completion watchdog checked between
//! bounded polls instead of an unbounded wait.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use frame_capture::{CaptureContext, CaptureSession, CapturedFrame};
use frame_contracts::{
    CaptureTarget, DisplayId, ErrorCode, FpsMode, RecorderError, RecoveryStatus, Region,
};
use frame_settings::RegionBounds;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BOX, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Query, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
};
use windows::core::{BOOL, Interface};

use crate::capture::WindowsCaptureSession;
use crate::preview::{
    PreviewHub, PreviewSource, ThumbnailGenerator, error_image, preview_payload_plan,
};
use crate::surface::{
    EncodeTexture, OwnedPayloadPlan, PayloadLifetime, PayloadOwner, SharedPayload, SlotCounts,
    SurfaceIdentity, SurfacePool, SurfaceSlot, WindowsSurface, plan_owned_payload,
};

/// Bounded wall-clock budget for the first completed capture surface of a region session.
const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);
/// How often preparation re-checks for that first surface.
const PREPARATION_POLL: Duration = Duration::from_millis(2);
/// GPU-progress watchdog for one crop copy. A driver call cannot be forcibly cancelled, so
/// an expired copy is reported as a failure instead of being waited out.
const CROP_WATCHDOG: Duration = Duration::from_secs(5);
/// The one pixel format this path copies, identical to the capture surface format.
const PIXEL_FORMAT: &str = "DXGI_FORMAT_B8G8R8A8_UNORM";

/// A preparation cancelled before the region session became ready.
fn cancelled_error() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "Region capture preparation was cancelled before the session became ready."
            .to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new region session when ready; no native session was left running."
            .to_string(),
    }
}

/// An unexpected condition on the region path.
fn internal_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Stop the region session and start a new one; if it persists, report the Windows build, GPU and driver."
            .to_string(),
    }
}

/// The selected display or its surface geometry changed under the region session.
fn source_lost_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Stop the session, enumerate displays again and start a new region session."
            .to_string(),
    }
}

/// Maps one native failure of the crop path onto the accepted taxonomy.
///
/// Losing the device is a lost source; a refused or unsupported call is explicit instead of
/// being retried into a silent fallback.
fn map_native_error(api: &str, error: windows::core::Error) -> RecorderError {
    let code = error.code();
    let hresult = code.0;
    if code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
    {
        return source_lost_error(format!(
            "{api} failed with HRESULT 0x{hresult:08X}: the capture device or its duplication is no longer usable."
        ));
    }
    if code == E_ACCESSDENIED {
        return RecorderError {
            code: ErrorCode::PermissionDenied,
            message: format!(
                "{api} failed with HRESULT 0x{hresult:08X}: access to the capture device was denied."
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Run Frame from an interactive desktop session that has access to the selected display."
                .to_string(),
        };
    }
    internal_error(format!("{api} failed with HRESULT 0x{hresult:08X}."))
}

/// Bounded delivery counters of one region session.
#[derive(Default)]
struct RegionCounters {
    /// Completed display surfaces this session consumed.
    source_frames: AtomicU64,
    /// Crop copies submitted to the immediate context.
    crops_submitted: AtomicU64,
    /// Crop copies whose completion query signalled.
    crops_completed: AtomicU64,
    /// Crop copies refused by a native call.
    crop_copy_failures: AtomicU64,
    /// Crop copies that did not complete inside [`CROP_WATCHDOG`].
    crop_watchdog_expiries: AtomicU64,
    /// Copies that could not start because every crop slot was still held.
    crop_pool_exhausted: AtomicU64,
    /// Crop slots reclaimed by an unwind instead of an explicit path.
    discarded_on_unwind: AtomicU64,
    /// Nanoseconds of the last completed copy, for diagnostics only.
    last_copy_nanos: AtomicU64,
}

impl RegionCounters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn load(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// Scoped ownership of one claimed crop slot.
///
/// The crop copy is synchronous, so there is no pending list: the guard either hands its
/// slot to the lease it built or returns it to the pool, and a panic between those steps
/// still returns the slot and counts the discard.
struct CropClaim {
    slot: Option<Arc<SurfaceSlot>>,
    counters: Arc<RegionCounters>,
}

impl CropClaim {
    /// Claims one free crop slot for a copy. `None` means all four are still held.
    fn claim(pool: &SurfacePool, counters: &Arc<RegionCounters>) -> Option<Self> {
        let slot = pool.try_acquire_copy_slot()?;
        Some(Self {
            slot: Some(slot),
            counters: Arc::clone(counters),
        })
    }

    fn slot(&self) -> &Arc<SurfaceSlot> {
        self.slot
            .as_ref()
            .expect("a crop claim is only disarmed once")
    }

    fn into_slot(mut self) -> Arc<SurfaceSlot> {
        self.slot
            .take()
            .expect("a crop claim is only disarmed once")
    }

    /// Returns the slot after a failed copy. The caller counts the reason.
    fn fail_copy(mut self) {
        if let Some(slot) = self.slot.take() {
            slot.mark_free();
        }
    }
}

impl Drop for CropClaim {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.mark_free();
            RegionCounters::bump(&self.counters.discarded_on_unwind);
        }
    }
}

/// The lease-visible GPU backing of one crop surface set: the capture device plus its
/// bounded crop textures.
///
/// It deliberately holds no immediate context and no query, so a crop lease that outlives
/// its session can never touch the session's context. The capture payload is retained for
/// the whole lifetime: Direct3D resources belong to their device, and the crop textures
/// were created on the capture session's device.
struct CropBacking {
    _source_backing: SharedPayload,
    /// Address of the capture device, carried as a value: the backing must stay `Send +
    /// Sync`, and a raw pointer is not. It is rebuilt into a typed pointer only where the
    /// encoder view is produced, exactly like the hardware encoder's own preparation does.
    device_address: usize,
    textures: Vec<ID3D11Texture2D>,
    width: u32,
    height: u32,
    /// The crop pool this backing belongs to. The encoder compares it, so a crop lease can
    /// never be registered as a capture lease.
    pool: u64,
    lifetime: Arc<PayloadLifetime>,
}

impl PayloadOwner for CropBacking {
    fn encode_texture(&self, slot_index: usize) -> Option<EncodeTexture> {
        let texture = self.textures.get(slot_index)?;
        Some(EncodeTexture {
            device: self.device_address as *mut c_void,
            texture: texture.as_raw(),
            identity: SurfaceIdentity {
                pool: self.pool,
                device: self.device_address,
                texture: texture.as_raw() as usize,
                width: self.width,
                height: self.height,
            },
        })
    }
}

impl Drop for CropBacking {
    fn drop(&mut self) {
        self.lifetime.owner_dropped();
    }
}

/// The session-owned crop engine: the immediate context of the capture device, one
/// completion query per slot and the bounded texture set.
struct CropPool {
    copy_started: Option<Instant>,
    witness: crate::witness::Witness,
    context: ID3D11DeviceContext,
    queries: Vec<ID3D11Query>,
    backing: Arc<CropBacking>,
    slots: Arc<SurfacePool>,
    plan: OwnedPayloadPlan,
    bounds: RegionBounds,
    /// The geometry the capture session resolved, for the copy's own precondition checks.
    capture_width: u32,
    capture_height: u32,
    counters: Arc<RegionCounters>,
}

/// One reduced image at a time, using the crop pool's own device and context.
struct RegionPreview {
    gpu_image: Result<Arc<crate::gpu_preview::SharedImage>, String>,
    hub: Arc<PreviewHub>,
    generator: Option<ThumbnailGenerator>,
    device: ID3D11Device,
    owned_bytes: u64,
    context: ID3D11DeviceContext,
    pixels: Vec<u8>,
    last_queued: Option<Instant>,
    last_media: Option<u64>,
    last_epoch: u64,
}

struct PreviewContextGuard(ID3D11Multithread);
impl Drop for PreviewContextGuard {
    fn drop(&mut self) {
        unsafe { self.0.Leave() };
    }
}

impl RegionPreview {
    fn poll(&mut self) {
        if !self.hub.cpu_preview_allowed() || self.hub.gpu_sink().is_some() || !self.hub.enabled() {
            if let Some(generator) = self.generator.as_mut() {
                generator.discard_pending();
            }
            return;
        }
        if !self.generator.as_ref().is_some_and(|g| g.has_pending()) {
            return;
        }
        let outcome = self.with_context(|this| {
            this.generator
                .as_mut()
                .expect("checked above")
                .complete_pending(&mut this.pixels)
        });
        match outcome {
            Ok(Some((image, _))) => {
                let _ = self.hub.publish(image);
            }
            Ok(None) => {}
            Err(message) => {
                self.generator
                    .as_mut()
                    .expect("checked above")
                    .discard_pending();
                self.hub.publish_error(error_image(0, 0, None, message));
            }
        }
    }

    fn queue(&mut self, texture: &ID3D11Texture2D, width: u32, height: u32, media_ns: u64) {
        self.poll();
        let gpu = self.hub.gpu_sink();
        let epoch = self.hub.subscription_epoch();
        if epoch != self.last_epoch {
            self.last_epoch = epoch;
            self.last_media = None;
        }
        if gpu.is_none() && (!self.hub.cpu_preview_allowed() || !self.hub.enabled()) {
            self.last_media = None;
            self.hub.note_skipped();
            return;
        }
        if self.last_media == Some(media_ns) {
            return;
        }
        if self
            .last_queued
            .is_some_and(|at| at.elapsed() < self.hub.config().min_interval)
        {
            self.hub.note_rate_limited();
            return;
        }
        if let Some(sink) = gpu {
            match &self.gpu_image {
                Ok(image) if sink.prepare_image(image) => {}
                Ok(_) => return,
                Err(error) => {
                    sink.fail(error.clone());
                    return;
                }
            }
            // Copy the completed crop directly on its capture device. The shared
            // texture never retains a crop lease and the keyed mutex never waits.
            // Enter the same context guard used by the existing thumbnail path.
            let multi: Result<ID3D11Multithread, _> = self.context.cast();
            match multi {
                Ok(multi) => {
                    unsafe { multi.Enter() };
                    let _guard = PreviewContextGuard(multi);
                    let source = PreviewSource::new(texture, width, height);
                    if let Err(error) =
                        sink.submit(&source, &self.device, &self.context, self.owned_bytes)
                    {
                        sink.fail(error);
                    }
                }
                Err(error) => sink.fail(format!("Region preview context is unavailable: {error}")),
            }
            self.last_queued = Some(Instant::now());
            self.last_media = Some(media_ns);
            return;
        }
        let source = PreviewSource::new(texture, width, height);
        match self.with_context(|this| {
            if this.generator.is_none() {
                this.generator = Some(ThumbnailGenerator::create(
                    Some(this.device.clone()),
                    Some(this.context.clone()),
                    this.hub.config(),
                    this.owned_bytes,
                    this.hub.counters_handle(),
                )?);
            }
            this.generator
                .as_mut()
                .expect("created above")
                .queue(source, Some(media_ns))
        }) {
            Ok(true) => {
                self.last_queued = Some(Instant::now());
                self.last_media = Some(media_ns);
            }
            Ok(false) => {}
            Err(message) => {
                self.last_queued = Some(Instant::now());
                self.last_media = Some(media_ns);
                self.hub
                    .publish_error(error_image(0, 0, Some(media_ns), message));
            }
        }
    }

    fn with_context<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> Result<T, crate::preview::PreviewError>,
    ) -> Result<T, String> {
        let multi: ID3D11Multithread = self.context.cast().map_err(|error| {
            map_native_error(
                "ID3D11DeviceContext::QueryInterface(ID3D11Multithread)",
                error,
            )
            .message
        })?;
        unsafe { multi.Enter() };
        let _guard = PreviewContextGuard(multi);
        work(self).map_err(|error| error.message())
    }
}

impl CropPool {
    /// Allocates the crop engine for one canonical region on the capture device.
    ///
    /// The device is taken from the lease that backs the capture session, so the copies and
    /// the encoder registration happen on one device. The payload is preflighted before any
    /// texture exists.
    fn open(
        source: &WindowsSurface,
        bounds: RegionBounds,
        capture_width: u32,
        capture_height: u32,
        counters: &Arc<RegionCounters>,
        lifetime: &Arc<PayloadLifetime>,
    ) -> Result<Self, RecorderError> {
        let view = source.encode_texture().ok_or_else(|| {
            internal_error(
                "The capture lease does not expose the Direct3D device the region crop needs."
                    .to_string(),
            )
        })?;
        if view.device.is_null() {
            return Err(internal_error(
                "The capture lease reported no Direct3D device for the region crop.".to_string(),
            ));
        }
        if view.identity.width != capture_width || view.identity.height != capture_height {
            return Err(source_lost_error(format!(
                "The capture lease is {}x{} but the session was prepared for {}x{}; the region was not copied.",
                view.identity.width, view.identity.height, capture_width, capture_height
            )));
        }
        let width = bounds.width();
        let height = bounds.height();
        let plan = plan_owned_payload(width, height, false)?;
        let pool = SurfacePool::new(plan.slots_bytes);
        let device_address = view.identity.device;
        // SAFETY: the address comes from this live capture lease, whose payload the crop
        // backing below retains for its whole lifetime; the borrowed interface is only used
        // to create this session's own resources and to obtain its immediate context.
        let device: &ID3D11Device = unsafe { ID3D11Device::from_raw_borrowed(&view.device) }
            .ok_or_else(|| {
                internal_error(
                    "The capture lease exposed a null Direct3D device for the region crop."
                        .to_string(),
                )
            })?;
        // SAFETY: the device is alive for the call and the immediate context it returns is
        // a device singleton that the returned (owned) reference keeps alive.
        let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
            .map_err(|error| map_native_error("ID3D11Device::GetImmediateContext", error))?;
        let texture_desc = D3D11_TEXTURE2D_DESC {
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
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let query_desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut textures = Vec::with_capacity(plan.slot_count());
        let mut queries = Vec::with_capacity(plan.slot_count());
        for _ in 0..plan.slot_count() {
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: the descriptor is fully initialized, no initial data is passed and
            // the out-parameter is a valid optional the API fills on success.
            unsafe { device.CreateTexture2D(&texture_desc, None, Some(&mut texture)) }
                .map_err(|error| map_native_error("ID3D11Device::CreateTexture2D", error))?;
            let texture = texture.ok_or_else(|| {
                internal_error(format!(
                    "CreateTexture2D reported success without a {width}x{height} crop texture."
                ))
            })?;
            let mut query: Option<ID3D11Query> = None;
            // SAFETY: the descriptor is fully initialized and the API only writes into the
            // out-parameter. Each slot owns its query, so no poll can observe another
            // slot's completion.
            unsafe { device.CreateQuery(&query_desc, Some(&mut query)) }
                .map_err(|error| map_native_error("ID3D11Device::CreateQuery", error))?;
            let query = query.ok_or_else(|| {
                internal_error("CreateQuery reported success without a query.".to_string())
            })?;
            textures.push(texture);
            queries.push(query);
        }
        lifetime.owner_created();
        let binding = source.encode_backing();
        let backing = Arc::new(CropBacking {
            _source_backing: binding,
            device_address,
            textures,
            width,
            height,
            pool: pool.id(),
            lifetime: Arc::clone(lifetime),
        });
        Ok(Self {
            copy_started: None,
            witness: crate::witness::Witness::new()?,
            context,
            queries,
            backing,
            slots: pool,
            plan,
            bounds,
            capture_width,
            capture_height,
            counters: Arc::clone(counters),
        })
    }

    /// Copies the canonical region out of one completed capture surface.
    ///
    /// The source timestamp is not touched here: the caller keeps the capture session's own
    /// timestamp, so a cropped frame is the same instant of the same source. The capture
    /// lease is consumed and released once the copy has completed and been observed.
    fn crop(&mut self, source: WindowsSurface) -> Result<WindowsSurface, RecorderError> {
        let view = source.encode_texture().ok_or_else(|| {
            internal_error(
                "A capture surface did not expose the GPU backing the region crop needs."
                    .to_string(),
            )
        })?;
        if view.identity.device != self.backing.device_address {
            return Err(internal_error(format!(
                "A capture surface from device 0x{:X} was offered to a crop pool created on device 0x{:X}; cross-device copies are rejected.",
                view.identity.device, self.backing.device_address
            )));
        }
        if view.identity.width != self.capture_width || view.identity.height != self.capture_height
        {
            return Err(source_lost_error(format!(
                "A {}x{} capture surface arrived where the session resolved {}x{}; the region was not copied.",
                view.identity.width, view.identity.height, self.capture_width, self.capture_height
            )));
        }
        let Some(claim) = CropClaim::claim(&self.slots, &self.counters) else {
            RegionCounters::bump(&self.counters.crop_pool_exhausted);
            let state = self.slots.counts();
            return Err(internal_error(format!(
                "Every one of the {} crop slots is still held ({} lease(s) outstanding), so this update was not copied; a crop slot becomes reusable only after its consumer released its lease.",
                self.plan.slot_count(),
                state.consumer_owned
            )));
        };
        RegionCounters::bump(&self.counters.crops_submitted);
        let index = claim.slot().index();
        let Some(destination) = self.backing.textures.get(index) else {
            claim.fail_copy();
            RegionCounters::bump(&self.counters.crop_copy_failures);
            return Err(internal_error(format!(
                "Crop slot {index} has no texture in this pool."
            )));
        };
        let Some(query) = self.queries.get(index) else {
            claim.fail_copy();
            RegionCounters::bump(&self.counters.crop_copy_failures);
            return Err(internal_error(format!(
                "Crop slot {index} has no completion query in this pool."
            )));
        };
        let copy_box = copy_box(self.bounds)?;
        // SAFETY: this pointer comes from the same live capture payload the crop backing
        // retains for its whole lifetime, so the borrowed texture cannot dangle while the
        // copy is submitted.
        let Some(source_texture) = (unsafe { ID3D11Texture2D::from_raw_borrowed(&view.texture) })
        else {
            claim.fail_copy();
            RegionCounters::bump(&self.counters.crop_copy_failures);
            return Err(internal_error(
                "The capture surface exposed a null texture for the region crop.".to_string(),
            ));
        };
        let started = Instant::now();
        // SAFETY: the destination texture and the source texture were both created on this
        // device, the box is inside the source (validated against the capture geometry), the
        // per-slot query is ended after the copy on the same context, and this thread's use
        // of the shared immediate context is covered by the multithread protection the
        // capture session enabled and verified before it published any texture.
        unsafe {
            let phase = self.witness.enter(c"region/CopySubresourceRegion");
            self.context.CopySubresourceRegion(
                destination,
                0,
                0,
                0,
                0,
                source_texture,
                0,
                Some(&copy_box),
            );
            drop(phase);
            let phase = self.witness.enter(c"region/End");
            self.context.End(query);
            drop(phase);
            let _phase = self.witness.enter(c"region/Flush");
            self.context.Flush();
        }
        // Retain the source in the derived lease until the asynchronous copy
        // completes. The session polls this event once per owner turn.
        self.copy_started = Some(started);
        let slot = claim.into_slot();
        slot.mark_ready();
        slot.mark_consumer_owned();
        Ok(WindowsSurface::derived(
            slot,
            Arc::clone(&self.backing) as SharedPayload,
            source,
        ))
    }

    fn poll_complete(&mut self, surface: &WindowsSurface) -> Result<bool, RecorderError> {
        let query = &self.queries[surface.slot_index()];
        let mut complete = BOOL(0);
        let result = unsafe {
            let _phase = self.witness.enter(c"region/GetData");
            self.context.GetData(
                query,
                Some(&mut complete as *mut BOOL as *mut c_void),
                size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
        };
        if let Err(error) = result {
            RegionCounters::bump(&self.counters.crop_copy_failures);
            return Err(map_native_error(
                "ID3D11DeviceContext::GetData(crop event)",
                error,
            ));
        }
        let started = self
            .copy_started
            .expect("a pending crop owns its start time");
        if !complete.as_bool() {
            if started.elapsed() > CROP_WATCHDOG {
                RegionCounters::bump(&self.counters.crop_watchdog_expiries);
                return Err(internal_error(
                    "Region GPU copy exceeded its progress watchdog.".into(),
                ));
            }
            return Ok(false);
        }
        self.copy_started = None;
        self.counters.last_copy_nanos.store(
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        RegionCounters::bump(&self.counters.crops_completed);
        Ok(true)
    }

    /// One bounded line describing the crop engine's own GPU memory and occupancy.
    fn describe(&self) -> String {
        let counts = self.slots.counts();
        format!(
            "region: display-local physical pixels ({}, {}) to ({}, {}) = {}x{} {PIXEL_FORMAT}\n\
             capture geometry: {}x{} px; capture payload plan: {} bytes for {} slots (the display session preflights the same geometry)\n\
             crop payload plan: {} bytes for {} slots, computed separately from the capture payload\n\
             crop slots: free={} copy_pending={} ready_queued={} consumer_owned={}\n\
             crop counters: source_frames={} crops_submitted={} crops_completed={} copy_failures={} watchdog_expiries={} pool_exhausted={} discarded_on_unwind={} last_copy_ms={:.3}\n\
             crop payload owners: created={} live={}\n",
            self.bounds.left(),
            self.bounds.top(),
            self.bounds.right(),
            self.bounds.bottom(),
            self.bounds.width(),
            self.bounds.height(),
            self.capture_width,
            self.capture_height,
            self.plan.slots_bytes,
            self.plan.slot_count(),
            self.plan.slots_bytes,
            self.plan.slot_count(),
            counts.free,
            counts.copy_pending,
            counts.ready_queued,
            counts.consumer_owned,
            RegionCounters::load(&self.counters.source_frames),
            RegionCounters::load(&self.counters.crops_submitted),
            RegionCounters::load(&self.counters.crops_completed),
            RegionCounters::load(&self.counters.crop_copy_failures),
            RegionCounters::load(&self.counters.crop_watchdog_expiries),
            RegionCounters::load(&self.counters.crop_pool_exhausted),
            RegionCounters::load(&self.counters.discarded_on_unwind),
            RegionCounters::load(&self.counters.last_copy_nanos) as f64 / 1_000_000.0,
            self.backing.lifetime.created(),
            self.backing.lifetime.live(),
        )
    }
}

/// Maps a canonical half-open region onto the Direct3D box one copy uses.
///
/// `D3D11_BOX` is inclusive at `left`/`top`/`front` and exclusive at
/// `right`/`bottom`/`back`, which is the canonical rectangle itself: no edge is adjusted by
/// one pixel and the box is never clamped to the source.
fn copy_box(bounds: RegionBounds) -> Result<D3D11_BOX, RecorderError> {
    let convert = |value: i32, edge: &str| -> Result<u32, RecorderError> {
        u32::try_from(value).map_err(|_| {
            internal_error(format!(
                "The canonical region {edge} edge at {value} px cannot name a Direct3D box position."
            ))
        })
    };
    Ok(D3D11_BOX {
        left: convert(bounds.left(), "left")?,
        top: convert(bounds.top(), "top")?,
        front: 0,
        right: convert(bounds.right(), "right")?,
        bottom: convert(bounds.bottom(), "bottom")?,
        back: 1,
    })
}

/// Rechecks the canonical region against the geometry the capture session really resolved.
///
/// The enumerated and duplicated geometries come from two separate OS reads; this is the
/// post-start check that the rectangle still lies inside the surface that will be copied.
fn recheck_against_capture(
    bounds: RegionBounds,
    capture_width: u32,
    capture_height: u32,
) -> Result<(), RecorderError> {
    let (Ok(right), Ok(bottom)) = (i32::try_from(capture_width), i32::try_from(capture_height))
    else {
        return Err(internal_error(format!(
            "The capture geometry {capture_width}x{capture_height} does not fit the region coordinate space."
        )));
    };
    if bounds.right() > right || bounds.bottom() > bottom {
        return Err(source_lost_error(format!(
            "The region ({}, {}) to ({}, {}) does not fit the {}x{} surface the capture session resolved, so no pixel was copied.",
            bounds.left(),
            bounds.top(),
            bounds.right(),
            bounds.bottom(),
            capture_width,
            capture_height
        )));
    }
    Ok(())
}

/// One running region capture session over a selected display.
///
/// It is a [`CaptureSession`] whose surfaces are crop leases: the display session's own
/// timestamp and geometry rules are unchanged, and only the rectangle the holder asked for
/// is copied out of each completed surface.
pub struct RegionCaptureSession {
    pending_crop: Option<CapturedFrame<WindowsSurface>>,
    display: WindowsCaptureSession,
    region: Region,
    bounds: RegionBounds,
    /// The crop engine. Taken at stop: no further copy can start, while outstanding crop
    /// leases keep their own reference to the crop payload and to the device.
    crop: Option<CropPool>,
    preview: Option<RegionPreview>,
    last_crop: Option<(usize, u64)>,
    prepared: Option<CapturedFrame<WindowsSurface>>,
    /// The crop slot occupancy observed at stop, so diagnostics report the real final state
    /// of the released engine instead of a fabricated zero.
    stopped_slots: Option<SlotCounts>,
    lifetime: Arc<PayloadLifetime>,
    counters: Arc<RegionCounters>,
    /// The crop payload plan, kept after stop so diagnostics still report the memory that
    /// was reserved for this session.
    plan: OwnedPayloadPlan,
    stopped: bool,
    stop_result: Option<RecorderError>,
    failure: Option<RecorderError>,
}

impl std::fmt::Debug for RegionCaptureSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegionCaptureSession")
            .field("display", &self.region.display_id.0)
            .field(
                "region",
                &(
                    self.bounds.left(),
                    self.bounds.top(),
                    self.bounds.width(),
                    self.bounds.height(),
                ),
            )
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl RegionCaptureSession {
    /// Opens one real region session: the accepted display capture backend for the
    /// display the region names, plus a crop engine on that session's own device.
    ///
    /// The canonical region is validated before any native work, then rechecked against the
    /// geometry the capture session resolved, so a rectangle that is outside, empty or not
    /// encodable is an explicit error and never a clamped or scaled recording.
    pub fn start(region: &Region, context: CaptureContext) -> Result<Self, RecorderError> {
        Self::start_inner(region, context, None, true)
    }

    pub fn start_with_cursor(
        region: &Region,
        context: CaptureContext,
        show_cursor: bool,
    ) -> Result<Self, RecorderError> {
        Self::start_inner(region, context, None, show_cursor)
    }

    /// Starts a cropped preview on the same capture device and crop surface set.
    /// Preview errors after preparation are published through the hub and never
    /// replace a recording frame; no preview work runs without a subscriber.
    pub fn start_with_preview(
        region: &Region,
        context: CaptureContext,
        hub: Arc<PreviewHub>,
    ) -> Result<Self, RecorderError> {
        Self::start_with_preview_cursor(region, context, hub, true)
    }

    pub fn start_with_preview_cursor(
        region: &Region,
        context: CaptureContext,
        hub: Arc<PreviewHub>,
        show_cursor: bool,
    ) -> Result<Self, RecorderError> {
        hub.config().validate()?;
        Self::start_inner(region, context, Some(hub), show_cursor)
    }

    fn start_inner(
        region: &Region,
        context: CaptureContext,
        hub: Option<Arc<PreviewHub>>,
        show_cursor: bool,
    ) -> Result<Self, RecorderError> {
        if context.cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        let displays = crate::display::enumerate_displays()?;
        let display = displays
            .iter()
            .find(|display| display.id.0.eq_ignore_ascii_case(&region.display_id.0))
            .ok_or_else(|| {
                source_lost_error(format!(
                    "The region names display {}, which is not among the currently attached displays.",
                    region.display_id.0
                ))
            })?;
        let bounds = frame_settings::validate_region(region, display)?;
        let capture_width = display.width;
        let capture_height = display.height;
        let mut session = crate::capture::start_display_with_cursor(
            &CaptureTarget::Display(DisplayId(region.display_id.0.clone())),
            FpsMode::AutoMatchSource,
            context,
            show_cursor,
        )?;
        let started = Instant::now();
        let first = loop {
            match session.try_frame() {
                Ok(Some(frame)) => break frame,
                Ok(None) => {}
                Err(error) => {
                    let _ = session.stop();
                    return Err(error);
                }
            }
            if started.elapsed() >= PREPARATION_WATCHDOG {
                let error = internal_error(format!(
                    "The display session delivered no completed surface within {} ms, so the region session was not prepared.",
                    PREPARATION_WATCHDOG.as_millis()
                ));
                let _ = session.stop();
                return Err(error);
            }
            std::thread::sleep(PREPARATION_POLL);
        };
        // The capture session's descriptor geometry is authoritative for the copy; the
        // region was validated against the enumeration, so the two must agree here.
        if first.width != capture_width || first.height != capture_height {
            let error = source_lost_error(format!(
                "The display was enumerated as {capture_width}x{capture_height} but its capture session resolved {}x{}; the region session was not started.",
                first.width, first.height
            ));
            let _ = session.stop();
            return Err(error);
        }
        if let Err(error) = recheck_against_capture(bounds, first.width, first.height) {
            let _ = session.stop();
            return Err(error);
        }
        let preview_owned_bytes = if let Some(hub) = hub.as_ref() {
            let capture_bytes =
                plan_owned_payload(capture_width, capture_height, false)?.total_bytes;
            let crop_bytes =
                plan_owned_payload(bounds.width(), bounds.height(), false)?.total_bytes;
            let owned_bytes = capture_bytes.checked_add(crop_bytes).ok_or_else(|| {
                internal_error("The combined capture and crop payload size overflowed.".into())
            })?;
            preview_payload_plan(
                hub.config(),
                bounds.width(),
                bounds.height(),
                owned_bytes,
                0,
            )?;
            Some(owned_bytes)
        } else {
            None
        };
        let counters = Arc::new(RegionCounters::default());
        let lifetime = PayloadLifetime::new();
        let crop = match CropPool::open(
            &first.surface,
            bounds,
            first.width,
            first.height,
            &counters,
            &lifetime,
        ) {
            Ok(crop) => crop,
            Err(error) => {
                let _ = session.stop();
                return Err(error);
            }
        };
        let preview = if let Some(hub) = hub {
            let owned_bytes = preview_owned_bytes.expect("preflighted for an attached hub");
            let view = first.surface.encode_texture().ok_or_else(|| {
                internal_error("The capture surface has no device for region preview.".into())
            })?;
            let device = unsafe { ID3D11Device::from_raw_borrowed(&view.device) }
                .ok_or_else(|| internal_error("The region preview device is null.".into()))?;
            Some(RegionPreview {
                gpu_image: crate::gpu_preview::prepare_shared(
                    device,
                    bounds.width(),
                    bounds.height(),
                    owned_bytes,
                ),
                hub,
                generator: None,
                device: device.clone(),
                owned_bytes,
                context: crop.context.clone(),
                pixels: Vec::new(),
                last_queued: None,
                last_media: None,
                last_epoch: 0,
            })
        } else {
            None
        };
        // Retain the real first frame: a static display might deliver no second update.
        Ok(Self {
            pending_crop: None,
            display: session,
            region: region.clone(),
            bounds,
            plan: crop.plan,
            crop: Some(crop),
            preview,
            last_crop: None,
            prepared: Some(first),
            stopped_slots: None,
            lifetime,
            counters,
            stopped: false,
            stop_result: None,
            failure: None,
        })
    }

    /// The canonical region this session copies.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The canonical half-open bounds of that region inside the display.
    pub fn bounds(&self) -> RegionBounds {
        self.bounds
    }

    /// Human-readable region diagnostics.
    ///
    /// It reports the crop geometry, the crop payload bytes and the capture payload bytes
    /// separately, the crop slot occupancy, the bounded counters and the stop state, and
    /// then appends the display session's own diagnostics. Every section is a snapshot, not
    /// an atomic view of the whole session.
    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        let mut report = String::new();
        report.push_str("Frame Windows region capture diagnostics (FRAME-0205)\n");
        report.push_str(&format!(
            "requested: display_id={} region=({}, {}) {}x{} (canonical display-local physical pixels, half-open)\n",
            self.region.display_id.0,
            self.region.x,
            self.region.y,
            self.region.width,
            self.region.height
        ));
        match &self.crop {
            Some(crop) => report.push_str(&crop.describe()),
            None => {
                // After stop the engine is released; the reserved bytes, the occupancy
                // observed at stop and the payload owners are still reported from this
                // session's own records.
                let counts = self.stopped_slots.unwrap_or_default();
                report.push_str(&format!(
                    "region: display-local physical pixels ({}, {}) to ({}, {}) = {}x{} {PIXEL_FORMAT}\n\
                     crop engine released at stop; crop payload plan: {} bytes for {} slots\n\
                     crop slots at stop: free={} copy_pending={} ready_queued={} consumer_owned={} (a crop lease still held at stop keeps its texture alive)\n\
                     crop counters: source_frames={} crops_submitted={} crops_completed={} copy_failures={} watchdog_expiries={} pool_exhausted={} discarded_on_unwind={}\n\
                     crop payload owners: created={} live={}\n",
                    self.bounds.left(),
                    self.bounds.top(),
                    self.bounds.right(),
                    self.bounds.bottom(),
                    self.bounds.width(),
                    self.bounds.height(),
                    self.plan.slots_bytes,
                    self.plan.slot_count(),
                    counts.free,
                    counts.copy_pending,
                    counts.ready_queued,
                    counts.consumer_owned,
                    RegionCounters::load(&self.counters.source_frames),
                    RegionCounters::load(&self.counters.crops_submitted),
                    RegionCounters::load(&self.counters.crops_completed),
                    RegionCounters::load(&self.counters.crop_copy_failures),
                    RegionCounters::load(&self.counters.crop_watchdog_expiries),
                    RegionCounters::load(&self.counters.crop_pool_exhausted),
                    RegionCounters::load(&self.counters.discarded_on_unwind),
                    self.lifetime.created(),
                    self.lifetime.live(),
                ));
            }
        }
        report.push_str(&format!(
            "stop: stopped={} failure={:?} stop_result={:?}\n",
            self.stopped,
            self.failure.as_ref().map(|error| error.message.clone()),
            self.stop_result.as_ref().map(|error| error.message.clone())
        ));
        report.push('\n');
        report.push_str(&self.display.diagnostics()?);
        Ok(report)
    }
}

impl CaptureSession for RegionCaptureSession {
    type Surface = WindowsSurface;

    fn set_wake_signal(&mut self, wake: frame_capture::PipelineWake) {
        self.display.set_wake_signal(wake);
    }
    fn set_replay_sampling(&mut self, enabled: bool) {
        self.display.set_replay_sampling(enabled);
    }

    fn pipeline_stats(&self) -> Option<frame_capture::CapturePipelineStats> {
        self.display.pipeline_stats()
    }

    /// Starts or polls one asynchronous region copy, without a sleep or completion wait.
    ///
    /// The returned frame keeps the capture session's own timestamp and reports the crop
    /// geometry, never the display geometry. `Ok(None)` means no ready source update — the
    /// session stays live and nothing is fabricated from it.
    fn try_frame(&mut self) -> Result<Option<CapturedFrame<WindowsSurface>>, RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.stopped {
            return Ok(None);
        }
        if let (Some(preview), Some((index, media_ns)), Some(crop)) =
            (self.preview.as_mut(), self.last_crop, self.crop.as_ref())
            && let Some(texture) = crop.backing.textures.get(index)
        {
            // A viewer can attach after the last display update. The texture stays
            // owned by this crop pool; no capture lease is retained for preview.
            preview.queue(texture, crop.bounds.width(), crop.bounds.height(), media_ns);
        }
        let Some(crop) = self.crop.as_mut() else {
            return Err(internal_error(
                "The region session has no crop engine although it was not stopped.".to_string(),
            ));
        };
        let outcome: Result<Option<CapturedFrame<WindowsSurface>>, RecorderError> = (|| {
            if self.pending_crop.is_none() {
                let frame = match self.prepared.take() {
                    Some(frame) => Some(frame),
                    None => self.display.try_frame()?,
                };
                let Some(frame) = frame else {
                    return Ok(None);
                };
                RegionCounters::bump(&self.counters.source_frames);
                let timestamp = frame.timestamp;
                let surface = crop.crop(frame.surface)?;
                self.pending_crop = Some(CapturedFrame {
                    surface,
                    timestamp,
                    width: crop.bounds.width(),
                    height: crop.bounds.height(),
                });
            }
            let pending = self
                .pending_crop
                .as_ref()
                .expect("pending crop was prepared");
            if !crop.poll_complete(&pending.surface)? {
                return Ok(None);
            }
            let frame = self.pending_crop.take().expect("completed pending crop");
            let index = frame.surface.slot_index();
            self.last_crop = Some((index, frame.timestamp.0));
            if let Some(preview) = self.preview.as_mut()
                && let Some(texture) = crop.backing.textures.get(index)
            {
                preview.queue(texture, frame.width, frame.height, frame.timestamp.0);
            }
            Ok(Some(frame))
        })();
        if let Err(error) = &outcome {
            // A failed copy is sticky: the session reports the same failure instead of
            // silently continuing with a partly copied stream of frames.
            self.failure.get_or_insert_with(|| error.clone());
        }
        outcome
    }

    /// Idempotent teardown on a control thread.
    ///
    /// The display session is stopped first, so no further surface can be produced, and the
    /// crop engine is then released on this thread: with no crop lease outstanding, the crop
    /// textures, the queries and this session's reference to the capture device go away
    /// here, while outstanding crop leases stay valid. Diagnostics remain available after
    /// the stop.
    fn stop(&mut self) -> Result<(), RecorderError> {
        if !self.stopped {
            self.stopped = true;
            self.prepared = None;
            if let Some(preview) = self.preview.take() {
                preview.hub.close();
            }
            self.last_crop = None;
            // Stop the display first: after this no further surface can be produced, so the
            // crop engine below is released without any copy still starting.
            let mut failure = self.display.stop().err();
            self.pending_crop = None;
            if let Some(crop) = self.crop.take() {
                self.stopped_slots = Some(crop.slots.counts());
            }
            if failure.is_none() {
                failure = self.failure.clone();
            }
            self.stop_result = failure;
        }
        match &self.stop_result {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl Drop for RegionCaptureSession {
    fn drop(&mut self) {
        // Safety net only: stops the display worker and releases the crop engine. It never
        // detaches a worker and never invalidates an outstanding crop lease.
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::SlotState;
    use frame_contracts::{DisplayId, DisplayInfo};

    fn display(width: u32, height: u32) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId("display-1".into()),
            name: "test panel".into(),
            width,
            height,
            x: 0,
            y: 0,
            scale_factor: 1.0,
            refresh_rate_hz: Some(60.0),
            is_primary: true,
            hdr_available: false,
            required_recording_quality: None,
        }
    }

    #[test]
    fn asynchronous_crop_keeps_source_owned_and_copies_the_exact_rectangle() {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::*;
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .unwrap();
        let device = device.unwrap();
        let context = context.unwrap();
        let mut pixels = Vec::new();
        for y in 0..8u8 {
            for x in 0..8u8 {
                pixels.extend_from_slice(&[x, y, 37, 255]);
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 8,
            Height: 8,
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
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: 32,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }.unwrap();
        let pool = SurfacePool::new(plan_owned_payload(8, 8, false).unwrap().slots_bytes);
        let slot = pool.try_acquire_copy_slot().unwrap();
        slot.mark_ready();
        slot.mark_consumer_owned();
        let backing = crate::surface::GpuSurfaceSet::new(
            device.clone(),
            vec![texture.unwrap()],
            8,
            8,
            pool.id(),
            PayloadLifetime::new(),
        );
        let source = WindowsSurface::new(slot, backing);
        let region = Region {
            display_id: DisplayId("display-1".into()),
            x: 2,
            y: 2,
            width: 4,
            height: 4,
        };
        let bounds = frame_settings::validate_region(&region, &display(8, 8)).unwrap();
        let counters = Arc::new(RegionCounters::default());
        let mut crop =
            CropPool::open(&source, bounds, 8, 8, &counters, &PayloadLifetime::new()).unwrap();
        let output = crop.crop(source).unwrap();
        assert_eq!(
            counters.crops_completed.load(Ordering::Relaxed),
            0,
            "submission returns before observing completion"
        );
        assert_eq!(pool.counts().consumer_owned, 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !crop.poll_complete(&output).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(counters.crops_completed.load(Ordering::Relaxed), 1);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 4,
            Height: 4,
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..desc
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.unwrap();
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // Readback is fixture-only; the production crop remains GPU-only.
        unsafe {
            context.CopyResource(&staging, &crop.backing.textures[output.slot_index()]);
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
        }
        for y in 0..4usize {
            for x in 0..4usize {
                let offset = y * mapped.RowPitch as usize + x * 4;
                let actual = unsafe {
                    std::slice::from_raw_parts((mapped.pData as *const u8).add(offset), 4)
                };
                assert_eq!(actual, &[(x + 2) as u8, (y + 2) as u8, 37, 255]);
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        drop(output);
        assert_eq!(pool.counts().free, crate::surface::SLOT_COUNT);
    }

    #[test]
    fn the_canonical_rectangle_maps_onto_the_d3d_box_without_moving_an_edge() {
        let panel = display(2560, 1600);
        let region = Region {
            display_id: panel.id.clone(),
            x: 240,
            y: 900,
            width: 640,
            height: 360,
        };
        let bounds = frame_settings::validate_region(&region, &panel).expect("bounds");
        let copy = copy_box(bounds).expect("a box");
        // Inclusive left/top, exclusive right/bottom: exactly the canonical rectangle.
        assert_eq!((copy.left, copy.top), (240, 900));
        assert_eq!((copy.right, copy.bottom), (880, 1260));
        assert_eq!(copy.right - copy.left, bounds.width());
        assert_eq!(copy.bottom - copy.top, bounds.height());
        assert_eq!((copy.front, copy.back), (0, 1));
    }

    #[test]
    fn capture_and_crop_payloads_are_computed_separately_and_bounded() {
        let plan = plan_owned_payload(2560, 1600, false).expect("capture plan");
        let crop = plan_owned_payload(640, 360, false).expect("crop plan");
        assert_eq!(crop.slots_bytes, 640 * 360 * 4 * 4);
        assert!(crop.slots_bytes < plan.slots_bytes);
        assert_eq!(crop.staging_bytes, 0, "the crop path reads nothing back");
        // A crop that cannot fit the accepted per-set budget is refused before allocation.
        assert!(plan_owned_payload(16384, 16384, false).is_err());
    }

    #[test]
    fn the_post_start_recheck_refuses_a_region_the_resolved_surface_cannot_hold() {
        let panel = display(2560, 1600);
        let region = Region {
            display_id: panel.id.clone(),
            x: 240,
            y: 900,
            width: 640,
            height: 360,
        };
        let bounds = frame_settings::validate_region(&region, &panel).expect("bounds");
        assert!(recheck_against_capture(bounds, 2560, 1600).is_ok());
        // The duplicated surface is smaller than the enumerated display: the region is
        // refused instead of being clamped or copied partially.
        let error = recheck_against_capture(bounds, 1920, 1080)
            .expect_err("a region outside the resolved surface is refused");
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(
            error.message.contains("no pixel was copied"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_crop_claim_returns_its_slot_on_every_path_and_counts_an_unwind() {
        let panel = display(640, 480);
        let region = Region {
            display_id: panel.id.clone(),
            x: 0,
            y: 0,
            width: 320,
            height: 240,
        };
        let bounds = frame_settings::validate_region(&region, &panel).expect("bounds");
        let plan = plan_owned_payload(bounds.width(), bounds.height(), false).expect("plan");
        let pool = SurfacePool::new(plan.slots_bytes);
        let counters = Arc::new(RegionCounters::default());

        let claim = CropClaim::claim(&pool, &counters).expect("a free crop slot");
        let index = claim.slot().index();
        assert_eq!(pool.counts().copy_pending, 1);
        claim.fail_copy();
        assert_eq!(pool.counts().free, plan.slot_count());
        assert!(index < plan.slot_count());

        let unwind = CropClaim::claim(&pool, &counters).expect("a free crop slot");
        drop(unwind);
        assert_eq!(pool.counts().free, plan.slot_count());
        assert_eq!(RegionCounters::load(&counters.discarded_on_unwind), 1);

        // Claiming every slot makes the next claim explicit rather than allocating a fifth.
        let mut held = Vec::new();
        for _ in 0..plan.slot_count() {
            held.push(CropClaim::claim(&pool, &counters).expect("a free crop slot"));
        }
        assert!(CropClaim::claim(&pool, &counters).is_none());
        for claim in held {
            claim.into_slot().mark_free();
        }
        assert_eq!(pool.counts().free, plan.slot_count());
        assert_eq!(
            pool.counts().free + pool.counts().occupied(),
            plan.slot_count()
        );
    }

    #[test]
    fn a_crop_lease_owns_its_source_and_a_textureless_pool_exposes_no_encoder_view() {
        let plan = plan_owned_payload(320, 240, false).expect("plan");
        let pool = SurfacePool::new(plan.slots_bytes);
        let lifetime = PayloadLifetime::new();
        lifetime.owner_created();
        let backing: SharedPayload = Arc::new(CropBacking {
            _source_backing: Arc::new(SyntheticPayload),
            device_address: 0x1000,
            textures: Vec::new(),
            width: 320,
            height: 240,
            pool: pool.id(),
            lifetime: Arc::clone(&lifetime),
        });
        let source_slot = pool.try_acquire_copy_slot().expect("a free slot");
        source_slot.mark_ready();
        source_slot.mark_consumer_owned();
        let source = WindowsSurface::new(Arc::clone(&source_slot), Arc::clone(&backing));
        let crop_slot = pool.try_acquire_copy_slot().expect("a free slot");
        crop_slot.mark_ready();
        crop_slot.mark_consumer_owned();
        let crop = WindowsSurface::derived(Arc::clone(&crop_slot), Arc::clone(&backing), source);
        assert!(crop.is_derived());
        assert_eq!(crop_slot.state(), SlotState::ConsumerOwned);
        assert_eq!(source_slot.state(), SlotState::ConsumerOwned);
        // A crop pool with no GPU textures reports no encoder view at all, so a synthetic
        // backing can never be registered as an encoder input.
        assert!(crop.encode_texture().is_none());
        drop(crop);
        assert_eq!(crop_slot.state(), SlotState::Free);
        assert_eq!(source_slot.state(), SlotState::Free);
        assert_eq!(lifetime.live(), 1, "the backing is still owned here");
        drop(backing);
        assert_eq!(lifetime.live(), 0, "destruction is observed, not inferred");
    }

    /// A payload owner with no GPU backing, for the lease tests only.
    struct SyntheticPayload;

    impl PayloadOwner for SyntheticPayload {
        fn encode_texture(&self, _slot_index: usize) -> Option<EncodeTexture> {
            None
        }
    }
}
