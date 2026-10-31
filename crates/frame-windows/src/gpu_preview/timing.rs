//! Opt-in fixed storage for CPU-observed preview call/queue durations.
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PreviewStageTiming {
    pub samples: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    /// Logarithmic microsecond buckets. Percentiles are upper bounds.
    pub buckets: [u64; 32],
}

impl PreviewStageTiming {
    pub fn percentile_upper_us(&self, percentile: u8) -> Option<u64> {
        if self.samples == 0 || !(1..=100).contains(&percentile) {
            return None;
        }
        let rank = (u128::from(self.samples) * u128::from(percentile)).div_ceil(100);
        let mut seen = 0u128;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += u128::from(*count);
            if seen >= rank {
                return Some((1u64 << index).max(if index == 31 {
                    self.max_ns.div_ceil(1000)
                } else {
                    0
                }));
            }
        }
        None // A live snapshot can race an observation; never fabricate a percentile.
    }
}

#[derive(Default)]
pub(super) struct Timing {
    samples: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; 32],
}
impl Timing {
    pub(super) fn record(&self, nanos: u64) {
        let micros = nanos.div_ceil(1000).max(1);
        let index = (64 - (micros - 1).leading_zeros()).min(31) as usize;
        self.total_ns.fetch_add(nanos, Ordering::Relaxed);
        self.max_ns.fetch_max(nanos, Ordering::Relaxed);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.samples.fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn snapshot(&self) -> PreviewStageTiming {
        PreviewStageTiming {
            samples: self.samples.load(Ordering::Relaxed),
            total_ns: self.total_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_timing_bounds_do_not_understate_tail_or_invent_missing_samples() {
        let timing = Timing::default();
        assert_eq!(timing.snapshot().percentile_upper_us(95), None);
        for nanos in [0, 1000, 1001, 4000, 4001, u64::MAX / 2] {
            timing.record(nanos);
        }
        let stats = timing.snapshot();
        assert_eq!(stats.samples, 6);
        assert_eq!(stats.percentile_upper_us(50), Some(2));
        assert_eq!(
            stats.percentile_upper_us(99),
            Some((u64::MAX / 2).div_ceil(1000))
        );
        assert_eq!(stats.percentile_upper_us(0), None);
    }
}
