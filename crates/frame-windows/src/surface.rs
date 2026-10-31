//! Frame-owned GPU surface leases (FRAME-0103, ADR 0005).
//!
//! The public type in this module is a non-cloneable lease over one of a session's
//! preallocated texture slots. It exposes no COM object, no raw handle and no
//! immediate context. Its `Drop` only returns its slot to its own session's pool, so
//! it can never hand a released resource to another session; the opaque payload owner
//! it holds keeps the backing texture and device alive until the last lease is gone,
//! including after the session stopped.
//!
//! Slot states follow ADR 0005: `Free -> CopyPending -> ReadyQueued -> ConsumerOwned
//! -> Free`, with explicit discard transitions back to `Free`. Slots are transitions
//! over a per-session fixed set; no path allocates another slot because a consumer
//! stalled.
//!
//! The module also owns the two pieces of evidence that FRAME-0103-R1 requires not to
//! be inferred: the preflighted [`OwnedPayloadPlan`] that must exist before any
//! texture is allocated, and the [`PayloadLifetime`] owner counters that prove the
//! GPU payload was actually destroyed after a stop.

use std::ffi::c_void;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use frame_encode::VideoColour;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

use crate::capture::diagnostics::Counters;

/// Preallocated slots per session (ADR 0005: four, including pending, queued and
/// consumer-held slots).
pub(crate) const SLOT_COUNT: usize = 4;

/// Pool identities, so the encoder can reject a lease from another capture pool.
static POOL_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Ready queue capacity per session (ADR 0005: two, `DropOldest`).
pub(crate) const READY_QUEUE_CAPACITY: usize = 2;

/// Frame-owned texture payload budget per session (ADR 0005: 512 MiB).
pub(crate) const MAX_PAYLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Preflighted Frame-owned payload for one session.
///
/// A plan can only be produced by [`plan_owned_payload`], so allocation code cannot
/// run before the combined budget (slots **and** the opt-in staging texture) has been
/// checked. Every field is checked arithmetic; overflow is a rejection, not a wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnedPayloadPlan {
    pub(crate) slots_bytes: u64,
    pub(crate) staging_bytes: u64,
    pub(crate) total_bytes: u64,
}

impl OwnedPayloadPlan {
    pub(crate) fn slot_count(&self) -> usize {
        SLOT_COUNT
    }
}

/// Checks the complete owned payload before any texture is allocated.
///
/// Four slots of a `width`×`height` BGRA8 surface must fit the budget on their own,
/// and the opt-in staging texture must fit in addition to them. Neither rule may be
/// evaluated after a texture exists, because a rejected request must allocate nothing.
pub(crate) fn plan_owned_payload(
    width: u32,
    height: u32,
    include_staging: bool,
) -> Result<OwnedPayloadPlan, RecorderError> {
    plan_owned_payload_with_bytes(width, height, include_staging, 4)
}

pub(crate) fn plan_owned_payload_with_bytes(
    width: u32,
    height: u32,
    include_staging: bool,
    bytes_per_pixel: u64,
) -> Result<OwnedPayloadPlan, RecorderError> {
    let per_row = u64::from(width)
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| budget_error(width, height, include_staging, "the row size overflowed"))?;
    let per_texture = per_row.checked_mul(u64::from(height)).ok_or_else(|| {
        budget_error(
            width,
            height,
            include_staging,
            "the texture size overflowed",
        )
    })?;
    let slots_bytes = per_texture
        .checked_mul(u64::try_from(SLOT_COUNT).unwrap_or(u64::MAX))
        .ok_or_else(|| {
            budget_error(
                width,
                height,
                include_staging,
                "the slot payload overflowed",
            )
        })?;
    if slots_bytes > MAX_PAYLOAD_BYTES {
        return Err(budget_error(
            width,
            height,
            include_staging,
            "four owned surfaces alone exceed the session payload budget",
        ));
    }
    let staging_bytes = if include_staging { per_texture } else { 0 };
    let total_bytes = slots_bytes.checked_add(staging_bytes).ok_or_else(|| {
        budget_error(
            width,
            height,
            include_staging,
            "the total payload overflowed",
        )
    })?;
    if total_bytes > MAX_PAYLOAD_BYTES {
        return Err(budget_error(
            width,
            height,
            include_staging,
            "the surfaces plus the verification staging texture exceed the session payload budget",
        ));
    }
    Ok(OwnedPayloadPlan {
        slots_bytes,
        staging_bytes,
        total_bytes,
    })
}

fn budget_error(width: u32, height: u32, include_staging: bool, detail: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "A {width}x{height} capture surface set ({SLOT_COUNT} slots{}, {MAX_PAYLOAD_BYTES} byte budget) was rejected before allocation: {detail}.",
            if include_staging {
                " plus a verification staging texture"
            } else {
                ""
            }
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Capture a smaller display or disable pixel verification; Frame does not allocate part of an oversized surface set."
            .to_string(),
    }
}

/// Live accounting for Frame-owned payload owners.
///
/// Diagnostics report it so that destruction can be *proved* after stop (live count
/// returns to zero) instead of being inferred from slot labels, which do not observe
/// GPU object lifetime.
#[derive(Default)]
pub(crate) struct PayloadLifetime {
    live: AtomicI64,
    created: AtomicU64,
}

impl PayloadLifetime {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records that one payload owner now exists. Owners call this from their
    /// constructor and [`PayloadLifetime::owner_dropped`] from `Drop`.
    pub(crate) fn owner_created(&self) {
        self.created.fetch_add(1, Ordering::Relaxed);
        self.live.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn owner_dropped(&self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn live(&self) -> i64 {
        self.live.load(Ordering::Acquire)
    }

    pub(crate) fn created(&self) -> u64 {
        self.created.load(Ordering::Relaxed)
    }
}

/// One slot's state in the session-owned slot machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum SlotState {
    /// Available for a new copy.
    Free = 0,
    /// A copy into this slot is in flight or its query has not signalled yet.
    CopyPending = 1,
    /// The copy completed and the slot is queued for a consumer.
    ReadyQueued = 2,
    /// A consumer holds a lease over this slot.
    ConsumerOwned = 3,
}

impl SlotState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::CopyPending,
            2 => Self::ReadyQueued,
            3 => Self::ConsumerOwned,
            _ => Self::Free,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::CopyPending => "copy-pending",
            Self::ReadyQueued => "ready-queued",
            Self::ConsumerOwned => "consumer-owned",
        }
    }
}

/// One slot's state. The slot itself holds no GPU object: the native payload owner
/// keeps the textures and queries indexed by slot, so the state machine and lease
/// rules stay independent of the GPU and testable.
pub(crate) struct SurfaceSlot {
    index: usize,
    state: AtomicU8,
}

/// Where in the copy lifecycle a claim was taken, deciding how an *unwinding* discard
/// is classified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SlotStage {
    /// Claimed for a copy that has not completed yet.
    BeforeCopy,
    /// A completed copy whose slot has not been published or discarded yet.
    AfterCopy,
}

/// Scoped ownership of one claimed slot.
///
/// Every transfer of a slot goes through this guard: claiming for a copy, taking a
/// completed copy, publishing it, or discarding it on a classified error path. If the
/// worker unwinds between two of those steps, `Drop` returns the slot to the pool and
/// counts the discards, so a panic cannot strand a surface (FRAME-0103-R2 finding 1).
/// The guard only touches atomics, so it cannot panic a second time while unwinding.
pub(crate) struct SlotClaim {
    slot: Option<Arc<SurfaceSlot>>,
    stage: SlotStage,
    counters: Arc<Counters>,
}

impl SlotClaim {
    /// Claims a free slot for a copy and counts the attempt.
    ///
    /// `None` means all slots are occupied: the caller drops the update instead of
    /// allocating another surface.
    pub(crate) fn for_copy(pool: &SurfacePool, counters: &Arc<Counters>) -> Option<Self> {
        let slot = pool.try_acquire_copy_slot()?;
        Counters::bump(&counters.copy_submissions);
        Some(Self {
            slot: Some(slot),
            stage: SlotStage::BeforeCopy,
            counters: Arc::clone(counters),
        })
    }

    /// Takes ownership of a completed copy's slot.
    pub(crate) fn for_completed(slot: Arc<SurfaceSlot>, counters: &Arc<Counters>) -> Self {
        Self {
            slot: Some(slot),
            stage: SlotStage::AfterCopy,
            counters: Arc::clone(counters),
        }
    }

    pub(crate) fn index(&self) -> usize {
        self.slot.as_ref().map(|slot| slot.index()).unwrap_or(0)
    }

    /// The claimed slot, for marking it ready or building a ready frame.
    pub(crate) fn slot(&self) -> &Arc<SurfaceSlot> {
        self.slot
            .as_ref()
            .expect("a slot claim is only disarmed once")
    }

    /// Transfers ownership out of the guard, e.g. into the pending list or a published
    /// ready frame.
    pub(crate) fn into_slot(mut self) -> Arc<SurfaceSlot> {
        self.slot
            .take()
            .expect("a slot claim is only disarmed once")
    }

    /// Classified discards. Each returns the slot exactly once and counts why.
    pub(crate) fn fail_copy(mut self) {
        self.release(|counters| &counters.copy_failures);
    }

    pub(crate) fn fail_verification(mut self) {
        self.release(|counters| &counters.discarded_on_verification_failure);
    }

    pub(crate) fn fail_contention(mut self) {
        self.release(|counters| &counters.worker_contention_drops);
    }

    pub(crate) fn fail_queue_poison(mut self) {
        self.release(|counters| &counters.discarded_on_queue_poison);
    }

    pub(crate) fn fail_cancellation(mut self) {
        self.release(|counters| &counters.discarded_on_cancellation);
    }

    fn release(&mut self, select: impl Fn(&Counters) -> &AtomicU64) {
        if let Some(slot) = self.slot.take() {
            slot.mark_free();
            Counters::bump(select(&self.counters));
        }
    }
}

impl Drop for SlotClaim {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.mark_free();
            match self.stage {
                // An incomplete copy: the discard belongs to the pending accounting.
                SlotStage::BeforeCopy => {
                    Counters::bump(&self.counters.discarded_on_unwind_pending);
                }
                // A completed copy that never reached a consumer: the discard belongs
                // to the delivery accounting.
                SlotStage::AfterCopy => {
                    Counters::bump(&self.counters.discarded_on_unwind_completed);
                }
            }
        }
    }
}

impl SurfaceSlot {
    pub(crate) fn index(&self) -> usize {
        self.index
    }

    pub(crate) fn state(&self) -> SlotState {
        SlotState::from_raw(self.state.load(Ordering::Acquire))
    }

    /// Claims a free slot for a new copy. `false` means another owner has it.
    pub(crate) fn try_begin_copy(&self) -> bool {
        self.state
            .compare_exchange(
                SlotState::Free as u8,
                SlotState::CopyPending as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Marks a completed copy as queued for a consumer.
    pub(crate) fn mark_ready(&self) {
        self.state
            .store(SlotState::ReadyQueued as u8, Ordering::Release);
    }

    /// Marks a queued slot as held by a consumer lease.
    pub(crate) fn mark_consumer_owned(&self) {
        self.state
            .store(SlotState::ConsumerOwned as u8, Ordering::Release);
    }

    /// Returns a slot to the pool. Used by the lease, and by explicit discard
    /// transitions for dropped, evicted or shutdown-discarded content.
    pub(crate) fn mark_free(&self) {
        self.state.store(SlotState::Free as u8, Ordering::Release);
    }
}

/// One session's fixed slot set.
pub(crate) struct SurfacePool {
    slots: Vec<Arc<SurfaceSlot>>,
    payload_bytes: u64,
    /// Stable identity of this pool, so the encoder can reject cross-pool input.
    id: u64,
}

impl SurfacePool {
    pub(crate) fn new(payload_bytes: u64) -> Arc<Self> {
        let slots = (0..SLOT_COUNT)
            .map(|index| {
                Arc::new(SurfaceSlot {
                    index,
                    state: AtomicU8::new(SlotState::Free as u8),
                })
            })
            .collect();
        Arc::new(Self {
            slots,
            payload_bytes,
            id: POOL_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// The stable identity of this session's pool.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Claims any free slot for a copy. `None` means all four are occupied and the
    /// incoming update must be dropped instead of allocating another surface.
    pub(crate) fn try_acquire_copy_slot(&self) -> Option<Arc<SurfaceSlot>> {
        self.slots
            .iter()
            .find(|slot| slot.try_begin_copy())
            .cloned()
    }

    pub(crate) fn slot_state(&self, index: usize) -> Option<SlotState> {
        self.slots.get(index).map(|slot| slot.state())
    }

    pub(crate) fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    pub(crate) fn counts(&self) -> SlotCounts {
        let mut counts = SlotCounts::default();
        for slot in &self.slots {
            match slot.state() {
                SlotState::Free => counts.free += 1,
                SlotState::CopyPending => counts.copy_pending += 1,
                SlotState::ReadyQueued => counts.ready_queued += 1,
                SlotState::ConsumerOwned => counts.consumer_owned += 1,
            }
        }
        counts
    }
}

/// Occupancy of one session's four slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SlotCounts {
    pub(crate) free: usize,
    pub(crate) copy_pending: usize,
    pub(crate) ready_queued: usize,
    pub(crate) consumer_owned: usize,
}

impl SlotCounts {
    pub(crate) fn occupied(&self) -> usize {
        self.copy_pending + self.ready_queued + self.consumer_owned
    }
}

/// Opaque ownership of the Frame-owned GPU payload: the device and its textures.
/// Queries, immediate context and optional staging remain worker-owned. The trait's only
/// operation is the private, typed encoder view below; holding an `Arc` of it is what
/// keeps the payload valid after the session stopped.
pub(crate) trait PayloadOwner: Send + Sync {
    /// Private, typed access to one slot's texture for the hardware encoder
    /// (FRAME-0104).
    ///
    /// `None` means this owner has no GPU backing at all — a synthetic payload in tests
    /// — and must never be treated as an encoder input. The view carries the device and
    /// the slot's texture plus the identities the encoder checks before registering
    /// anything, and it is never reachable through a public type or a shared contract.
    fn encode_texture(&self, slot_index: usize) -> Option<EncodeTexture>;
    fn format(&self) -> SurfaceFormat {
        SurfaceFormat::Bgra8
    }
    fn signal(&self) -> Option<SurfaceSignal> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceFormat {
    Bgra8,
    /// Native two-plane 4:2:0 textures, passed directly to supported encoders.
    Nv12,
    P010,
    /// DXGI_FORMAT_R16G16B16A16_FLOAT in linear scRGB; 1.0 represents SDR white.
    ScRgbFloat16,
}

/// Known source signal for the actual pixels in the lease. Display output colour
/// describes the monitor; the FP16 desktop image itself is linear scRGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceSignal {
    pub colour: VideoColour,
    pub reference_white_nits: u16,
}

impl SurfaceSignal {
    pub const SCRGB: Self = Self {
        colour: VideoColour {
            bits_per_channel: 16,
            matrix_coefficients: 0,
            transfer_characteristics: 8,
            primaries: 1,
            range: 2,
            content_light: None,
            mastering: None,
        },
        reference_white_nits: 80,
    };
}

pub(crate) type SharedPayload = Arc<dyn PayloadOwner>;

/// Four owned GPU textures that can back either a display or WGC window session.
/// Leases retain this owner after the capture worker has stopped.
pub(crate) struct GpuSurfaceSet {
    device: ID3D11Device,
    textures: Vec<ID3D11Texture2D>,
    width: u32,
    height: u32,
    pool: u64,
    lifetime: Arc<PayloadLifetime>,
}

impl GpuSurfaceSet {
    pub(crate) fn new(
        device: ID3D11Device,
        textures: Vec<ID3D11Texture2D>,
        width: u32,
        height: u32,
        pool: u64,
        lifetime: Arc<PayloadLifetime>,
    ) -> Arc<Self> {
        lifetime.owner_created();
        Arc::new(Self {
            device,
            textures,
            width,
            height,
            pool,
            lifetime,
        })
    }

    pub(crate) fn texture(&self, slot_index: usize) -> Option<&ID3D11Texture2D> {
        self.textures.get(slot_index)
    }
    pub(crate) fn device(&self) -> ID3D11Device {
        self.device.clone()
    }
}

impl PayloadOwner for GpuSurfaceSet {
    fn encode_texture(&self, slot_index: usize) -> Option<EncodeTexture> {
        let texture = self.textures.get(slot_index)?;
        let device = self.device.as_raw();
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

impl Drop for GpuSurfaceSet {
    fn drop(&mut self) {
        self.lifetime.owner_dropped();
    }
}

/// Identity of one capture pool, device and texture slot.
///
/// These are stable comparison values, not a public handle: the encoder compares them to
/// reject a lease from another pool, another device or another geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SurfaceIdentity {
    pub(crate) pool: u64,
    pub(crate) device: usize,
    pub(crate) texture: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Private typed view of a lease's GPU backing for the encoder.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EncodeTexture {
    /// `ID3D11Device*` of the capture path.
    pub(crate) device: *mut c_void,
    /// `ID3D11Texture2D*` of one completed capture slot.
    pub(crate) texture: *mut c_void,
    pub(crate) identity: SurfaceIdentity,
}

/// A lease over one Frame-owned texture slot.
///
/// Non-cloneable and opaque: no COM object, raw pointer or immediate context is
/// reachable, and it cannot be recycled into another session because the slot it
/// returns to belongs to the pool that created it.
///
/// A *derived* lease (FRAME-0205) also holds the lease it was derived from, so the source
/// slot stays owned for exactly as long as the derived one exists. The region crop path
/// uses this: a cropped surface is copied out of a capture slot on the same device, and
/// holding the source lease is what keeps the capture session's bounded drop policy — not a
/// new failure mode — as the only place where pressure is resolved. It adds no side table
/// and no reference cycle.
pub struct WindowsSurface {
    preparation_only: bool,
    slot: Arc<SurfaceSlot>,
    /// Keeps the session's bounded payload alive for as long as this lease exists.
    /// The session's fixed slot set is retained, never grown.
    _payload: SharedPayload,
    /// The leased surface this one was derived from, if any. Dropped with this lease, so
    /// the source slot returns to its own pool at the same moment. Boxed: a lease holding a
    /// lease of its own type needs indirection.
    _source: Option<Box<WindowsSurface>>,
}

impl WindowsSurface {
    /// The format actually held by this lease's texture, not the selected display's
    /// wire format. HDR conversion must use this distinction.
    pub fn format(&self) -> SurfaceFormat {
        self._payload.format()
    }
    pub fn signal(&self) -> Option<SurfaceSignal> {
        self._payload.signal()
    }
    pub(crate) fn new(slot: Arc<SurfaceSlot>, payload: SharedPayload) -> Self {
        Self {
            preparation_only: false,
            slot,
            _payload: payload,
            _source: None,
        }
    }

    pub(crate) fn preparation(slot: Arc<SurfaceSlot>, payload: SharedPayload) -> Self {
        Self {
            preparation_only: true,
            slot,
            _payload: payload,
            _source: None,
        }
    }

    pub(crate) fn is_preparation(&self) -> bool {
        self.preparation_only
    }

    /// Builds a derived lease that owns `source` for exactly its own lifetime.
    ///
    /// Used by the region crop path: the crop lease is a lease over its own bounded pool,
    /// and holding the source lease is what proves the copied region's source slot cannot be
    /// recycled while a consumer still holds the crop. Camera composition may add a
    /// second derived lease above a region crop, preserving both source slots.
    pub(crate) fn derived(
        slot: Arc<SurfaceSlot>,
        payload: SharedPayload,
        source: WindowsSurface,
    ) -> Self {
        Self {
            preparation_only: false,
            slot,
            _payload: payload,
            _source: Some(Box::new(source)),
        }
    }

    /// True when this lease was derived from another surface rather than copied by a
    /// capture session.
    pub(crate) fn is_derived(&self) -> bool {
        self._source.is_some()
    }

    /// Current slot state. Crate-private for tests and diagnostics.
    pub(crate) fn slot_state(&self) -> SlotState {
        self.slot.state()
    }

    pub(crate) fn slot_index(&self) -> usize {
        self.slot.index()
    }

    /// Private typed encoder view of this lease's texture.
    ///
    /// It exists so the encoder can register and map exactly this lease on the capture
    /// device; it is not a clone, not a COM clone API and not reachable publicly.
    pub(crate) fn encode_texture(&self) -> Option<EncodeTexture> {
        self._payload.encode_texture(self.slot.index())
    }

    /// The opaque payload owner this lease retains.
    ///
    /// The encoder holds one of these per native registration, so a registered texture
    /// and its device stay alive independently of any per-frame lease, until that
    /// registration has been released natively. It exposes no COM type and no pointer.
    pub(crate) fn encode_backing(&self) -> SharedPayload {
        Arc::clone(&self._payload)
    }
}

impl Drop for WindowsSurface {
    fn drop(&mut self) {
        // Thread-safe ownership return only: no native call, no blocking wait and no
        // channel hand-off, so a lease can be dropped on any consumer thread.
        self.slot.mark_free();
    }
}

impl fmt::Debug for WindowsSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsSurface")
            .field("slot", &self.slot.index())
            .field("state", &self.slot.state().name())
            .field("derived", &self.is_derived())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot bytes for a surface set, or None when the budget rejects it.
    fn slots_bytes(width: u32, height: u32) -> Option<u64> {
        plan_owned_payload(width, height, false)
            .ok()
            .map(|plan| plan.slots_bytes)
    }

    /// A synthetic payload owner that records its own creation and destruction.
    struct TestPayload {
        lifetime: Arc<PayloadLifetime>,
    }

    impl TestPayload {
        fn new(lifetime: &Arc<PayloadLifetime>) -> Self {
            lifetime.owner_created();
            Self {
                lifetime: Arc::clone(lifetime),
            }
        }
    }

    impl PayloadOwner for TestPayload {
        fn encode_texture(&self, _slot_index: usize) -> Option<EncodeTexture> {
            // A synthetic payload has no GPU backing, so the encoder must refuse it.
            None
        }
    }

    impl Drop for TestPayload {
        fn drop(&mut self) {
            self.lifetime.owner_dropped();
        }
    }

    #[test]
    fn payload_budget_rejects_oversized_surfaces() {
        assert_eq!(slots_bytes(2560, 1600), Some(65_536_000));
        assert_eq!(slots_bytes(3840, 2160), Some(132_710_400));
        // 7680x4320 BGRA8 x 4 slots (about 506 MiB) still fits the 512 MiB budget;
        // a marginally larger surface does not.
        assert_eq!(slots_bytes(7680, 4320), Some(530_841_600));
        assert!(plan_owned_payload(8192, 4320, false).is_err());
        // Overflow is rejected rather than wrapping.
        assert!(plan_owned_payload(u32::MAX, u32::MAX, false).is_err());
    }

    #[test]
    fn the_payload_plan_is_decided_before_any_allocation() {
        // Four 7680x4320 BGRA8 slots fit exactly inside the budget.
        let plan = plan_owned_payload(7680, 4320, false).expect("four slots fit");
        assert_eq!(plan.slots_bytes, 530_841_600);
        assert_eq!(plan.staging_bytes, 0);
        assert_eq!(plan.total_bytes, 530_841_600);
        assert!(plan.total_bytes <= MAX_PAYLOAD_BYTES);
        assert_eq!(plan.slot_count(), SLOT_COUNT);

        // A fifth surface of the same size does not fit, and a staging texture is
        // exactly that fifth surface: the combined request must be refused up front.
        assert_eq!(
            plan_owned_payload(7680, 4320, true)
                .expect_err("surfaces plus staging exceed the budget")
                .code,
            ErrorCode::Internal
        );
        // A surface set that cannot fit on its own is refused even without staging.
        assert!(plan_owned_payload(8192, 4320, false).is_err());
        assert!(plan_owned_payload(8192, 4320, true).is_err());
        // Overflow is a rejection, not a wrap.
        assert!(plan_owned_payload(u32::MAX, u32::MAX, true).is_err());

        // The ordinary reference panel fits with room for the staging texture.
        let plan = plan_owned_payload(2560, 1600, true).expect("reference panel fits");
        assert_eq!(plan.slots_bytes, 65_536_000);
        assert_eq!(plan.staging_bytes, 16_384_000);
        assert_eq!(plan.total_bytes, 81_920_000);
    }

    #[test]
    fn payload_lifetime_counts_owners_until_their_last_reference_drops() {
        let lifetime = PayloadLifetime::new();
        assert_eq!(lifetime.created(), 0);
        assert_eq!(lifetime.live(), 0);

        let owner: Arc<dyn PayloadOwner> = Arc::new(TestPayload::new(&lifetime));
        assert_eq!(lifetime.created(), 1);
        assert_eq!(lifetime.live(), 1);
        let second = Arc::clone(&owner);
        assert_eq!(lifetime.live(), 1, "cloning an owner is not a new owner");
        drop(owner);
        assert_eq!(lifetime.live(), 1);
        drop(second);
        assert_eq!(lifetime.live(), 0, "destruction is observed, not inferred");
        assert_eq!(lifetime.created(), 1);
    }

    #[test]
    fn slots_move_through_the_documented_states_and_return_to_free() {
        let pool = SurfacePool::new(plan_owned_payload(2560, 1600, false).unwrap().slots_bytes);
        let slot = pool.try_acquire_copy_slot().expect("a free slot");
        assert_eq!(slot.state(), SlotState::CopyPending);
        slot.mark_ready();
        assert_eq!(slot.state(), SlotState::ReadyQueued);

        let lease = WindowsSurface::new(
            slot.clone(),
            Arc::new(TestPayload::new(&PayloadLifetime::new())),
        );
        slot.mark_consumer_owned();
        assert_eq!(lease.slot_state(), SlotState::ConsumerOwned);
        assert_eq!(pool.counts().occupied(), 1);

        drop(lease);
        assert_eq!(slot.state(), SlotState::Free);
        assert_eq!(
            pool.counts(),
            SlotCounts {
                free: SLOT_COUNT,
                ..SlotCounts::default()
            }
        );
    }

    #[test]
    fn all_four_slots_can_be_occupied_and_the_fifth_request_is_refused() {
        let pool = SurfacePool::new(plan_owned_payload(640, 480, false).unwrap().slots_bytes);
        let mut leases = Vec::new();
        for _ in 0..SLOT_COUNT {
            let slot = pool.try_acquire_copy_slot().expect("a free slot");
            slot.mark_ready();
            slot.mark_consumer_owned();
            leases.push(WindowsSurface::new(
                slot,
                Arc::new(TestPayload::new(&PayloadLifetime::new())),
            ));
        }
        assert_eq!(pool.counts().consumer_owned, SLOT_COUNT);
        assert!(
            pool.try_acquire_copy_slot().is_none(),
            "a stalled consumer must not make the pool grow"
        );
        drop(leases);
        assert_eq!(pool.counts().free, SLOT_COUNT);
        assert!(pool.try_acquire_copy_slot().is_some());
    }

    #[test]
    fn a_lease_keeps_its_payload_alive_and_returns_only_its_own_slot() {
        let first = SurfacePool::new(plan_owned_payload(640, 480, false).unwrap().slots_bytes);
        let second = SurfacePool::new(plan_owned_payload(640, 480, false).unwrap().slots_bytes);

        let slot = first.try_acquire_copy_slot().expect("a free slot");
        slot.mark_ready();
        slot.mark_consumer_owned();
        let lease = WindowsSurface::new(
            slot.clone(),
            Arc::new(TestPayload::new(&PayloadLifetime::new())),
        );

        // Dropping the source session's pool handle does not invalidate the lease.
        drop(first);
        assert_eq!(lease.slot_state(), SlotState::ConsumerOwned);

        // A different session's pool is untouched by that lease.
        assert_eq!(second.counts().free, SLOT_COUNT);
        drop(lease);
        assert_eq!(slot.state(), SlotState::Free);
        assert_eq!(second.counts().free, SLOT_COUNT);
    }

    #[test]
    fn a_derived_lease_owns_its_source_for_exactly_its_own_lifetime() {
        let source_pool =
            SurfacePool::new(plan_owned_payload(640, 480, false).unwrap().slots_bytes);
        let crop_pool = SurfacePool::new(plan_owned_payload(320, 240, false).unwrap().slots_bytes);

        let source_slot = source_pool
            .try_acquire_copy_slot()
            .expect("a free source slot");
        source_slot.mark_ready();
        source_slot.mark_consumer_owned();
        let source = WindowsSurface::new(
            Arc::clone(&source_slot),
            Arc::new(TestPayload::new(&PayloadLifetime::new())),
        );
        assert!(!source.is_derived());

        let crop_slot = crop_pool.try_acquire_copy_slot().expect("a free crop slot");
        crop_slot.mark_ready();
        crop_slot.mark_consumer_owned();
        let crop = WindowsSurface::derived(
            Arc::clone(&crop_slot),
            Arc::new(TestPayload::new(&PayloadLifetime::new())),
            source,
        );
        assert!(crop.is_derived());
        // Both slots stay owned while the derived lease exists: the capture slot cannot be
        // copied into again until its crop was released.
        assert_eq!(source_slot.state(), SlotState::ConsumerOwned);
        assert_eq!(crop_pool.counts().consumer_owned, 1);

        drop(crop);
        assert_eq!(crop_slot.state(), SlotState::Free);
        assert_eq!(source_slot.state(), SlotState::Free);
        assert_eq!(source_pool.counts().free, SLOT_COUNT);
    }
}
