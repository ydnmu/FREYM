//! Source sample-clock speed against timestamps in the shared media timeline.
//! This observes raw PCM before nominal resampling. It never changes timestamps,
//! samples or capture pacing, and an estimate is not an A/V content sync test.

use crate::{MediaTimestamp, sample_timestamp};
use std::num::NonZeroU32;

const WINDOW_NS: u64 = 10_000_000_000;
const MAX_RATE_PPM: i64 = 5_000;
const MAX_STEP_RESIDUAL_NS: i128 = 100_000_000;
const STABLE_WINDOWS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriftEstimate {
    /// Positive means the source produces samples faster than its nominal rate.
    pub rate_ppm: i64,
    /// Source timestamp elapsed minus nominal sample elapsed, including startup
    /// offset removal. This is a clock residual, not measured content A/V error.
    pub residual_ns: i64,
    pub window_ns: u64,
    pub stable_windows: u32,
    pub reliable: bool,
}

#[derive(Clone, Copy)]
struct Observation {
    frames: u64,
    timestamp: MediaTimestamp,
    rate: NonZeroU32,
}

/// Fixed-size, allocation-free observer. Ten-second windows reject short-term
/// delivery jitter; three consistent windows are required before warnings.
#[derive(Default)]
pub struct ClockDriftEstimator {
    origin: Option<Observation>,
    window: Option<Observation>,
    previous: Option<Observation>,
    estimate: Option<DriftEstimate>,
    resets: u64,
}

impl ClockDriftEstimator {
    pub fn estimate(&self) -> Option<DriftEstimate> {
        self.estimate
    }

    pub fn resets(&self) -> u64 {
        self.resets
    }

    fn anchor(&mut self, value: Observation) {
        self.origin = Some(value);
        self.window = Some(value);
        self.previous = Some(value);
        self.estimate = None;
    }

    /// `frames` is the cumulative accepted source-frame count at the START of
    /// this raw block, whose native timestamp was mapped to the common clock.
    pub fn observe(
        &mut self,
        frames: u64,
        rate: NonZeroU32,
        timestamp: MediaTimestamp,
    ) -> Option<DriftEstimate> {
        let value = Observation {
            frames,
            timestamp,
            rate,
        };
        let Some(previous) = self.previous else {
            self.anchor(value);
            return None;
        };
        let continuous = (|| {
            if rate != previous.rate {
                return None;
            }
            let elapsed = timestamp.0.checked_sub(previous.timestamp.0)?;
            let nominal = sample_timestamp(frames.checked_sub(previous.frames)?, rate)?.0;
            let residual = i128::from(elapsed) - i128::from(nominal);
            let allowed = MAX_STEP_RESIDUAL_NS + i128::from(nominal) / 200;
            (residual.abs() <= allowed).then_some(())
        })()
        .is_some();
        if !continuous {
            self.resets = self.resets.saturating_add(1);
            self.anchor(value);
            return None;
        }
        self.previous = Some(value);
        let window = self.window?;
        let elapsed = timestamp.0.checked_sub(window.timestamp.0)?;
        if elapsed < WINDOW_NS {
            return self.estimate;
        }
        let nominal = sample_timestamp(frames.checked_sub(window.frames)?, rate)?.0;
        let ppm = (i128::from(nominal) - i128::from(elapsed)) * 1_000_000 / i128::from(elapsed);
        if ppm.abs() > i128::from(MAX_RATE_PPM) {
            self.resets = self.resets.saturating_add(1);
            self.anchor(value);
            return None;
        }
        let ppm = i64::try_from(ppm).ok()?;
        let stable = self.estimate.map_or(1, |last| {
            if last.rate_ppm.abs_diff(ppm) <= 100 {
                last.stable_windows.saturating_add(1)
            } else {
                1
            }
        });
        let origin = self.origin?;
        let total_elapsed = timestamp.0.checked_sub(origin.timestamp.0)?;
        let total_nominal = sample_timestamp(frames.checked_sub(origin.frames)?, rate)?.0;
        let estimate = DriftEstimate {
            rate_ppm: ppm,
            residual_ns: i64::try_from(i128::from(total_elapsed) - i128::from(total_nominal))
                .ok()?,
            window_ns: elapsed,
            stable_windows: stable,
            reliable: stable >= STABLE_WINDOWS,
        };
        self.window = Some(value);
        self.estimate = Some(estimate);
        Some(estimate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_initial_offset_from_one_hour_of_clock_speed_error() {
        let rate = NonZeroU32::new(48_000).unwrap();
        for ppm in [-200i64, 0, 200] {
            let mut observer = ClockDriftEstimator::default();
            let epoch = 1_700_000_000_123_456u64;
            for second in 0..=3600u64 {
                let nominal = second * 1_000_000_000;
                let elapsed = (u128::from(nominal) * 1_000_000
                    / u128::try_from(1_000_000 + ppm).unwrap())
                    as u64;
                observer.observe(second * 48_000, rate, MediaTimestamp(epoch + elapsed));
            }
            let estimate = observer.estimate().unwrap();
            assert!(estimate.reliable);
            assert!(estimate.rate_ppm.abs_diff(ppm) <= 1);
            if ppm == 0 {
                assert_eq!(estimate.residual_ns, 0);
            } else {
                assert!(estimate.residual_ns.unsigned_abs() > 700_000_000);
                assert_eq!(estimate.residual_ns.signum(), -ppm.signum());
            }
            assert_eq!(observer.resets(), 0);
        }
    }

    #[test]
    fn delivery_jitter_does_not_become_a_clock_speed_warning() {
        let rate = NonZeroU32::new(48_000).unwrap();
        let mut observer = ClockDriftEstimator::default();
        for tick in 0..=12_000u64 {
            let jitter = if tick % 2 == 0 { 250_000 } else { 750_000 };
            observer.observe(tick * 480, rate, MediaTimestamp(tick * 10_000_000 + jitter));
        }
        let estimate = observer.estimate().unwrap();
        assert!(estimate.reliable);
        assert!(estimate.rate_ppm.abs() <= 50);
        assert!(estimate.residual_ns.unsigned_abs() <= 500_000);
    }

    #[test]
    fn discontinuity_and_device_format_change_invalidate_old_estimates() {
        let rate = NonZeroU32::new(48_000).unwrap();
        let mut observer = ClockDriftEstimator::default();
        for second in 0..=40u64 {
            observer.observe(
                second * 48_000,
                rate,
                MediaTimestamp(second * 1_000_000_000),
            );
        }
        assert!(observer.estimate().unwrap().reliable);
        assert_eq!(
            observer.observe(41 * 48_000, rate, MediaTimestamp(45_000_000_000)),
            None
        );
        assert_eq!(observer.resets(), 1);
        assert_eq!(
            observer.observe(
                0,
                NonZeroU32::new(44_100).unwrap(),
                MediaTimestamp(46_000_000_000)
            ),
            None
        );
        assert_eq!(observer.resets(), 2);
    }
}
