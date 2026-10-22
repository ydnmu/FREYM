// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Explicit policies for clock correction and bounded same-device reconnect.

use frame_contracts::RecorderError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioContinuityPolicy {
    /// Disabled by default: the existing 48 kHz bypass keeps native PCM/timestamps.
    pub compensate_drift: bool,
    /// Reopen only the same stable source ID, never select another/default device.
    pub reconnect_source: bool,
}

impl AudioContinuityPolicy {
    pub fn from_environment() -> Result<Self, RecorderError> {
        Ok(Self {
            compensate_drift: read_flag("FREYM_AUDIO_DRIFT_COMPENSATION")?,
            reconnect_source: read_flag("FREYM_AUDIO_RECONNECT")?,
        })
    }
}

fn read_flag(name: &str) -> Result<bool, RecorderError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(false),
        Ok(value) if value == "0" => Ok(false),
        Ok(value) if value == "1" => Ok(true),
        _ => Err(RecorderError::invalid(format!(
            "{name} accepts only 0 or 1."
        ))),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriftCompensationHealth {
    pub enabled: bool,
    /// Actual correction passed to the sinc resampler, in thousandths of ppm.
    pub applied_milli_ppm: i64,
    pub reliable: bool,
}

/// Feed-forward device-rate estimate with a slow, bounded residual correction.
/// Delivery jitter alone cannot activate this controller. The output grid remains
/// anchored to the first source timestamp; only the sinc resampling ratio changes.
#[derive(Default)]
pub(crate) struct DriftController {
    applied_ppm: f64,
    previous_timestamp: Option<frame_clock::MediaTimestamp>,
    reliable: bool,
}

impl DriftController {
    pub(crate) fn update(
        &mut self,
        timestamp: frame_clock::MediaTimestamp,
        estimate: Option<frame_clock::DriftEstimate>,
        residual_ns: f64,
    ) -> f64 {
        let elapsed = self
            .previous_timestamp
            .and_then(|previous| timestamp.0.checked_sub(previous.0))
            .unwrap_or(0);
        self.previous_timestamp = Some(timestamp);
        self.reliable = estimate.is_some_and(|estimate| estimate.reliable);
        let target = estimate
            .filter(|estimate| estimate.reliable)
            .map_or(0.0, |estimate| {
                let feedback_ppm = (residual_ns / 60_000.0).clamp(-100.0, 100.0);
                (estimate.rate_ppm as f64 + feedback_ppm).clamp(-1_000.0, 1_000.0)
            });
        // At most 20 ppm per second, with no catch-up leap after a long gap.
        let step = 20.0 * (elapsed.min(100_000_000) as f64 / 1_000_000_000.0);
        self.applied_ppm += (target - self.applied_ppm).clamp(-step, step);
        1.0 / (1.0 + self.applied_ppm / 1_000_000.0)
    }

    pub(crate) fn health(&self) -> DriftCompensationHealth {
        DriftCompensationHealth {
            enabled: true,
            applied_milli_ppm: (self.applied_ppm * 1_000.0).round() as i64,
            reliable: self.reliable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_clock::{ClockDriftEstimator, MediaTimestamp};
    #[test]
    fn an_hour_of_fast_and_slow_clock_correction_stays_bounded() {
        for ppm in [-200i64, 200] {
            let mut estimate = ClockDriftEstimator::default();
            let mut control = DriftController::default();
            let mut frames = 0.0;
            for tick in 0..36_000u64 {
                let timestamp = MediaTimestamp(
                    (u128::from(tick * 100_000_000) * 1_000_000 / (1_000_000 + ppm) as u128) as u64,
                );
                let drift = estimate.observe(
                    tick * 4800,
                    std::num::NonZeroU32::new(48_000).unwrap(),
                    timestamp,
                );
                let residual = frames * 1e9 / 48_000.0 - timestamp.0 as f64;
                let ratio = control.update(timestamp, drift, residual);
                assert!((1.0 / 1.001..=1.0 / 0.999).contains(&ratio));
                frames += 4800.0 * ratio;
            }
            let duration = 3_600e9 * 1_000_000.0 / (1_000_000 + ppm) as f64;
            assert!((frames * 1e9 / 48_000.0 - duration).abs() < 1e6);
        }
    }
}
