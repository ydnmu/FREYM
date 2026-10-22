//! Measured encoder selection inputs. Measurements belong to native backends;
//! this policy never turns a missing value into a capability or a benchmark.

use crate::policy::{EncoderCandidate, NominalRate};
use frame_contracts::Quality;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderAssessment {
    /// Exact candidate ID is supplied alongside this value by the planner.
    pub completed_frames: u64,
    pub elapsed_ns: u64,
    pub failures: u32,
    /// CPU-observed encode/transfer p95, when the backend measured it.
    pub latency_p95_ns: Option<u64>,
    /// Measured source application p95 frame-time cost, when independently known.
    pub source_cost_p95_ns: Option<u64>,
    /// Per-frame CPU work in the recording/transfer path, excluding idle waits.
    pub recording_overhead_p95_ns: Option<u64>,
    /// Lost inputs, excluding deliberate FPS policy skips and retryable backpressure.
    pub dropped_frames: u64,
}

impl EncoderAssessment {
    /// Sustainable admission budget, including measured loss and per-frame work.
    /// Unknown values impose no ceiling. Latency is a ranking input because native
    /// asynchronous encoding may have several frames in flight.
    pub fn capacity_millihz(self) -> Option<u64> {
        let throughput = self.throughput_millihz()?;
        let delivered = u128::from(self.completed_frames);
        let attempted = delivered + u128::from(self.dropped_frames);
        let mut capacity = (u128::from(throughput) * delivered / attempted) as u64;
        if let Some(cost) = self.recording_overhead_p95_ns.filter(|n| *n != 0) {
            capacity = capacity.min(1_000_000_000_000 / cost);
        }
        Some(capacity)
    }
    pub fn throughput_millihz(self) -> Option<u64> {
        if self.completed_frames == 0 || self.elapsed_ns == 0 || self.failures != 0 {
            return None;
        }
        u64::try_from(
            u128::from(self.completed_frames) * 1_000_000_000_000 / u128::from(self.elapsed_ns),
        )
        .ok()
    }
}

/// Lexicographic preference: successful measured stability, nominal-rate
/// headroom, measured source cost, measured latency, then requested RC quality.
/// Unmeasured candidates remain available; no guessed vendor scores exist.
pub(crate) fn rank(
    candidate: &EncoderCandidate,
    assessment: Option<EncoderAssessment>,
    rate: NominalRate,
    quality: Quality,
) -> Option<(u8, u64, u64, u64, bool)> {
    let desired_rc = match quality {
        Quality::Standard => candidate.limits.cbr,
        Quality::High | Quality::Maximum => candidate.limits.vbr,
    };
    match assessment {
        Some(value) if value.failures != 0 => None,
        Some(value) => match value.capacity_millihz() {
            Some(throughput) => {
                let target =
                    (u64::from(rate.num().get()) * 1000).div_ceil(u64::from(rate.den().get()));
                if throughput < target {
                    return None;
                }
                // 20% headroom saturates the throughput preference so extreme
                // synthetic rates do not outrank a lower source-application cost.
                let headroom = throughput.saturating_mul(1000) / target;
                Some((
                    if headroom >= 1000 { 2 } else { 0 },
                    headroom.min(1200),
                    value.source_cost_p95_ns.map_or(0, |cost| u64::MAX - cost),
                    value.latency_p95_ns.map_or(0, |cost| u64::MAX - cost),
                    desired_rc,
                ))
            }
            None => Some((1, 0, 0, 0, desired_rc)),
        },
        None => Some((1, 0, 0, 0, desired_rc)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EncoderLimits,
        policy::{PlanInputs, plan_recording_with_assessments},
    };
    use frame_contracts::{Dimensions, EncoderPreference, FpsMode, Resolution};
    use std::num::NonZeroU32;

    #[test]
    fn auto_uses_real_evidence_and_excludes_failed_candidates() {
        let candidates = ["failed", "slow", "fast"].map(|id| EncoderCandidate {
            id: id.into(),
            codec: "h264".into(),
            hardware: true,
            limits: EncoderLimits {
                max_width: 1920,
                max_height: 1080,
                cbr: true,
                vbr: true,
            },
        });
        let inputs = PlanInputs {
            quality: Quality::Standard,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Auto,
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: NominalRate::new(
                NonZeroU32::new(60).unwrap(),
                NonZeroU32::new(1).unwrap(),
            ),
            candidates: &candidates,
        };
        let mut measurements = [
            (
                "failed".into(),
                EncoderAssessment {
                    completed_frames: 300,
                    elapsed_ns: 1_000_000_000,
                    failures: 1,
                    latency_p95_ns: None,
                    source_cost_p95_ns: None,
                    recording_overhead_p95_ns: None,
                    dropped_frames: 0,
                },
            ),
            (
                "slow".into(),
                EncoderAssessment {
                    completed_frames: 30,
                    elapsed_ns: 1_000_000_000,
                    failures: 0,
                    latency_p95_ns: Some(20_000_000),
                    source_cost_p95_ns: None,
                    recording_overhead_p95_ns: None,
                    dropped_frames: 0,
                },
            ),
            (
                "fast".into(),
                EncoderAssessment {
                    completed_frames: 90,
                    elapsed_ns: 1_000_000_000,
                    failures: 0,
                    latency_p95_ns: Some(8_000_000),
                    source_cost_p95_ns: None,
                    recording_overhead_p95_ns: None,
                    dropped_frames: 0,
                },
            ),
        ];
        let plan = plan_recording_with_assessments(&inputs, "h264", &measurements).unwrap();
        assert_eq!(plan.encoder.id, "fast");
        assert!(plan.decisions.iter().any(|d| d.detail.contains("90.000")));
        measurements[2].1.failures = 1;
        assert!(plan_recording_with_assessments(&inputs, "h264", &measurements).is_err());
        let mut explicit = inputs;
        explicit.encoder = EncoderPreference::Explicit("fast".into());
        assert!(plan_recording_with_assessments(&explicit, "h264", &measurements).is_err());
        // At a manually requested 24 FPS both candidates have enough headroom,
        // so the lower measured latency wins instead of using the 60 FPS source.
        explicit.encoder = EncoderPreference::Auto;
        explicit.fps_mode = FpsMode::Fixed(24.0);
        measurements[2].1.failures = 0;
        measurements[1].1.latency_p95_ns = Some(1_000_000);
        assert_eq!(
            plan_recording_with_assessments(&explicit, "h264", &measurements)
                .unwrap()
                .encoder
                .id,
            "slow"
        );
    }
}
