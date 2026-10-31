//! Private seam between the encoder's bounded policy and the native NVENC bridge
//! (FRAME-0104, ADR 0006).
//!
//! The worker owns admission, lease retention, packet credits, drain state and error
//! policy, and it never calls NVIDIA directly. Everything native is expressed by this
//! trait, whose production implementation is the C bridge in `encode/native.rs` and
//! whose test implementation is a scripted driver with no hardware at all. That keeps
//! pressure, credit, drain, failure and unwind behaviour testable without a GPU and
//! without a second encoder model.
//!
//! Nothing here is public: no COM pointer, SDK enum or raw video type leaves the crate
//! through this module.

use frame_contracts::RecorderError;

use super::NativeEncoderSettings;

/// Identity of one captured surface as the encoder sees it.
///
/// The pool and device values are stable identities compared to reject a lease that
/// does not belong to the capture pool this session was opened for; the texture value
/// identifies one of that pool's bounded slots. They are never pointer *content*: the
/// native driver is the only code that touches the pointers.
pub(crate) use crate::surface::{EncodeTexture, SurfaceIdentity};

/// The native answer to one submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeSubmit {
    /// The driver holds the picture; output buffer `slot` completes it.
    Pending { slot: u32 },
    /// The driver accepted and **buffered** the picture (`NV_ENC_ERR_NEED_MORE_INPUT`):
    /// the input stays mapped and output buffer `slot` and its event stay owned until
    /// that picture's access unit arrives, with later input or with end of stream. It is
    /// an outstanding submission, never a released input and never a completion.
    Buffered { slot: u32 },
    /// The driver was busy and accepted nothing; the input must be retried unchanged.
    Busy,
}

/// One complete Annex-B access unit copied out of the native bitstream buffer.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct NativePacket {
    pub(crate) data: Vec<u8>,
    /// The native output timestamp. Verified against the submitted source timestamp.
    pub(crate) pts_ns: u64,
    /// The native output duration. Verified against the submitted source duration.
    pub(crate) duration_ns: u64,
    /// A real native IDR flag from the locked bitstream, never inferred from a counter.
    pub(crate) keyframe: bool,
    pub(crate) picture_type: u32,
    pub(crate) frame_index: u64,
}

#[derive(Debug)]
pub(crate) enum NativeCollect {
    /// Not complete yet: keep the slot and its mapping.
    Pending,
    Ready(NativePacket),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeEos {
    /// The driver accepted the end-of-stream picture; its event must now be awaited.
    Submitted,
    /// The driver was busy and the end-of-stream picture must be retried.
    Busy,
}

/// Native call and retry counters, reported for diagnostics only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NativeStats {
    #[serde(default)]
    pub(crate) qp: Option<super::NativeQpStatistics>,
    pub(crate) native_calls: u32,
    pub(crate) busy_results: u32,
    pub(crate) pending_inputs: u32,
}

/// One initialized native encoder session, used by exactly one worker thread.
pub(crate) trait NvencDriver: Send {
    /// The effective configuration the driver reported after initialization.
    fn settings(&self) -> &NativeEncoderSettings;

    /// Registers one capture texture as a bounded Direct3D input image.
    fn register_input(&mut self, texture: EncodeTexture) -> Result<u32, RecorderError>;
    fn unregister_input(&mut self, registration: u32) -> Result<(), RecorderError>;

    /// Nonblocking submission. `force_idr` marks the first picture of the session.
    fn submit(
        &mut self,
        registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        force_idr: bool,
    ) -> Result<NativeSubmit, RecorderError>;

    /// Bounded wait (`wait_ms`) for one submitted picture, then a complete copy of its
    /// access unit into a buffer of at most `capacity` bytes. A larger access unit is an
    /// explicit failure and never a truncated packet.
    fn collect(
        &mut self,
        slot: u32,
        wait_ms: u32,
        capacity: usize,
    ) -> Result<NativeCollect, RecorderError>;

    /// Signals end of stream. Idempotent.
    fn begin_eos(&mut self) -> Result<NativeEos, RecorderError>;
    /// Bounded wait for end-of-stream acknowledgement.
    fn poll_eos(&mut self, wait_ms: u32) -> Result<bool, RecorderError>;

    fn stats(&self) -> NativeStats;

    fn mf_transfer_stats(&self) -> Option<super::MfTransferStats> {
        None
    }

    /// The exact native call this driver is inside right now, for diagnostics only.
    ///
    /// The production driver reads it from the bridge's bounded call-phase record, so a
    /// call that never returned is named from evidence rather than inferred from
    /// unchanged counters; a scripted driver reports that it has no native phases.
    fn phase(&self) -> String {
        "not reported by this driver".to_string()
    }

    /// Releases every native resource exactly once. Idempotent.
    ///
    /// `Err(CleanupUnsafe)` means the driver may still be using some resources: the caller
    /// must keep them represented and must not recycle the corresponding capture slots.
    fn close(&mut self) -> Result<(), RecorderError>;

    /// True when the last close could not establish that the driver stopped using the
    /// resources.
    fn cleanup_unsafe(&self) -> bool {
        false
    }
}
