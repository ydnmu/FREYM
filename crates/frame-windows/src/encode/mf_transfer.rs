//! Opt-in, bounded diagnostics for the CPU transfer across the isolated MF owner.
//! Timings are CPU-observed call durations; upload does not measure GPU completion.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Packed layout crossing the MF process boundary. The MFT input remains NV12.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MfTransferFormat {
    #[default]
    Bgra8,
    Nv12,
}

/// Shared GPU transport is opt-in until the adapter/driver has been accepted.
/// Its BGRA shared surface is copied/converts on the helper GPU, never mapped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MfTransferTransport {
    #[default]
    Cpu,
    SharedGpu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MfSharedFallback {
    ParentSharedResourceUnavailable,
    HelperSharedResourceUnavailable,
}

pub(super) fn selected_transport() -> Result<MfTransferTransport, frame_contracts::RecorderError> {
    match std::env::var_os("FREYM_MF_TRANSFER_TRANSPORT") {
        None => Ok(MfTransferTransport::Cpu),
        Some(value) if value == "cpu" => Ok(MfTransferTransport::Cpu),
        Some(value) if value == "shared_gpu" => Ok(MfTransferTransport::SharedGpu),
        Some(_) => Err(super::invalid_request_error(
            "FREYM_MF_TRANSFER_TRANSPORT must be cpu or shared_gpu.".into(),
        )),
    }
}

pub(super) fn selected_format() -> Result<MfTransferFormat, frame_contracts::RecorderError> {
    match std::env::var_os("FREYM_MF_TRANSFER_FORMAT") {
        None => Ok(MfTransferFormat::Bgra8),
        Some(value) if value == "bgra8" => Ok(MfTransferFormat::Bgra8),
        Some(value) if value == "nv12" => Ok(MfTransferFormat::Nv12),
        Some(_) => Err(super::invalid_request_error(
            "FREYM_MF_TRANSFER_FORMAT must be bgra8 or nv12.".into(),
        )),
    }
}

pub(super) fn enabled() -> bool {
    std::env::var_os("FREYM_MF_TRANSFER_METRICS").is_some_and(|value| value == "1")
}

/// A fixed logarithmic histogram. Percentiles are bucket upper bounds in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MfStageTiming {
    pub samples: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    buckets: [u64; 32],
}

impl MfStageTiming {
    pub(super) fn record(&mut self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let micros = nanos.div_ceil(1_000).max(1);
        let bucket = (64 - (micros - 1).leading_zeros() as usize).min(31);
        self.samples = self.samples.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(nanos);
        self.max_ns = self.max_ns.max(nanos);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }

    pub fn percentile_upper_us(&self, percentile: u8) -> Option<u64> {
        if self.samples == 0 || !(1..=100).contains(&percentile) {
            return None;
        }
        let rank = (u128::from(self.samples) * u128::from(percentile)).div_ceil(100);
        let mut seen = 0u128;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += u128::from(*count);
            if seen >= rank {
                return Some(if index == 31 {
                    (1u64 << index).max(self.max_ns.div_ceil(1_000))
                } else {
                    1u64 << index
                });
            }
        }
        None
    }
}

/// Transfer attempts include busy retries. IPC roundtrip includes helper work;
/// these overlapping timings must not be added as disjoint pipeline stages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MfTransferStats {
    #[serde(default)]
    pub requested_transport: MfTransferTransport,
    /// Registration counts describe actual routing, including mixed fallback.
    #[serde(default)]
    pub shared_gpu_registrations: u64,
    #[serde(default)]
    pub cpu_registrations: u64,
    #[serde(default)]
    pub last_shared_fallback: Option<MfSharedFallback>,
    #[serde(default)]
    pub shared_gpu_attempts: u64,
    #[serde(default)]
    pub shared_gpu_copy: MfStageTiming,
    #[serde(default)]
    pub helper_shared_gpu_copy: MfStageTiming,
    #[serde(default)]
    pub format: MfTransferFormat,
    #[serde(default)]
    pub packed_frame_bytes: u64,
    pub submit_attempts: u64,
    pub payload_bytes: u64,
    pub parent_buffer_allocations: u64,
    pub parent_buffer_capacity: u64,
    pub helper_received_frames: u64,
    pub helper_received_bytes: u64,
    pub helper_buffer_allocations: u64,
    pub helper_buffer_capacity: u64,
    pub readback: MfStageTiming,
    /// CPU duration of parent VideoProcessorBlt. GPU waits remain in readback.
    #[serde(default)]
    pub conversion: MfStageTiming,
    pub ipc_roundtrip: MfStageTiming,
    pub helper_receive: MfStageTiming,
    pub helper_upload: MfStageTiming,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(super) struct HelperTransferStats {
    #[serde(default)]
    pub shared_gpu_copy: MfStageTiming,
    pub received_frames: u64,
    pub received_bytes: u64,
    pub buffer_allocations: u64,
    pub buffer_capacity: u64,
    pub receive: MfStageTiming,
    pub upload: MfStageTiming,
}

impl MfTransferStats {
    pub(super) fn observe_helper(&mut self, helper: HelperTransferStats) {
        self.helper_received_frames = helper.received_frames;
        self.helper_received_bytes = helper.received_bytes;
        self.helper_buffer_allocations = helper.buffer_allocations;
        self.helper_buffer_capacity = helper.buffer_capacity;
        self.helper_receive = helper.receive;
        self.helper_upload = helper.upload;
        self.helper_shared_gpu_copy = helper.shared_gpu_copy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_bounds_cover_submicrosecond_values_and_long_stalls() {
        let mut timing = MfStageTiming::default();
        assert_eq!(timing.percentile_upper_us(95), None);
        for micros in [0, 1, 3, 4, 9] {
            timing.record(Duration::from_micros(micros));
        }
        assert_eq!(timing.percentile_upper_us(50), Some(4));
        assert_eq!(timing.percentile_upper_us(95), Some(16));
        assert_eq!(timing.percentile_upper_us(0), None);
        timing.record(Duration::from_secs(5_000));
        assert_eq!(timing.percentile_upper_us(100), Some(5_000_000_000));
    }
}
