//! Integer nanoseconds on one monotonic session timeline. Device clocks must
//! explicitly map into this timebase; wall-clock time is never a media timestamp.
use std::num::NonZeroU64;
use std::time::Instant;

pub mod drift;
pub use drift::{ClockDriftEstimator, DriftEstimate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MediaTimestamp(pub u64);

pub struct MasterClock {
    epoch: Instant,
}

/// Maps raw session timestamps to one edited media timeline shared by video and audio.
/// The owner discards native observations made during a pause. This map removes the
/// wall-clock interval without ever moving a previously admitted timestamp back.
#[derive(Debug, Default, Clone)]
pub struct PauseTimeline {
    removed_ns: u64,
    pause_at: Option<u64>,
    resume_after: Option<u64>,
}

impl PauseTimeline {
    pub fn pause(&mut self, raw: MediaTimestamp) -> Option<MediaTimestamp> {
        if self.pause_at.is_some() {
            return None;
        }
        let mapped = raw.0.checked_sub(self.removed_ns).map(MediaTimestamp)?;
        self.pause_at = Some(raw.0);
        Some(mapped)
    }
    pub fn resume(&mut self, raw: MediaTimestamp) -> Option<MediaTimestamp> {
        let pause = self.pause_at?;
        let removed = self.removed_ns.checked_add(raw.0.checked_sub(pause)?)?;
        let mapped = raw.0.checked_sub(removed).map(MediaTimestamp)?;
        self.removed_ns = removed;
        self.pause_at = None;
        self.resume_after = Some(raw.0);
        Some(mapped)
    }
    pub fn map(&self, raw: MediaTimestamp) -> Option<MediaTimestamp> {
        let value = if let Some(pause) = self.pause_at {
            raw.0.min(pause)
        } else {
            raw.0
        };
        value.checked_sub(self.removed_ns).map(MediaTimestamp)
    }
    pub fn is_paused(&self) -> bool {
        self.pause_at.is_some()
    }
    pub fn pause_boundary_raw(&self) -> Option<MediaTimestamp> {
        self.pause_at.map(MediaTimestamp)
    }
    pub fn resume_after(&self) -> Option<MediaTimestamp> {
        self.resume_after.map(MediaTimestamp)
    }
}
impl Default for MasterClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}
impl MasterClock {
    pub fn now(&self) -> MediaTimestamp {
        MediaTimestamp(u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }
}

/// One measured alignment of a native counter to the injected session clock.
/// This is an offset mapping, not device drift calibration (ADR 0005).
#[derive(Debug, Clone, Copy)]
pub struct ClockAnchor {
    native_ticks: u64,
    ticks_per_second: NonZeroU64,
    media_time: MediaTimestamp,
    uncertainty_ns: u64,
}

impl ClockAnchor {
    /// The native counter must have been sampled between `before` and `after`,
    /// both read from the same MasterClock. Includes one native tick of uncertainty.
    pub fn from_sample(
        native_ticks: u64,
        ticks_per_second: NonZeroU64,
        before: MediaTimestamp,
        after: MediaTimestamp,
    ) -> Option<Self> {
        let span = after.0.checked_sub(before.0)?;
        let tick_ns = 1_000_000_000u64.div_ceil(ticks_per_second.get());
        Some(Self {
            native_ticks,
            ticks_per_second,
            media_time: MediaTimestamp(before.0.checked_add(span / 2)?),
            uncertainty_ns: span.div_ceil(2).checked_add(tick_ns)?,
        })
    }

    /// Floor the signed counter difference to integer nanoseconds; reject times
    /// before the session epoch and overflow instead of clamping or wrapping.
    pub fn map(&self, native_ticks: u64) -> Option<MediaTimestamp> {
        let delta = i128::from(native_ticks) - i128::from(self.native_ticks);
        let elapsed_ns =
            (delta * 1_000_000_000).div_euclid(i128::from(self.ticks_per_second.get()));
        u64::try_from(i128::from(self.media_time.0) + elapsed_ns)
            .ok()
            .map(MediaTimestamp)
    }

    pub fn uncertainty_ns(&self) -> u64 {
        self.uncertainty_ns
    }
}

/// Exact sample-to-time conversion without accumulated floating-point rounding.
pub fn sample_timestamp(samples: u64, sample_rate: std::num::NonZeroU32) -> Option<MediaTimestamp> {
    let ns = u128::from(samples) * 1_000_000_000 / u128::from(sample_rate.get());
    u64::try_from(ns).ok().map(MediaTimestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_counter_maps_around_a_shared_epoch() {
        let anchor = ClockAnchor::from_sample(
            10_000,
            NonZeroU64::new(1_000).unwrap(),
            MediaTimestamp(2_000_000_000),
            MediaTimestamp(2_000_000_100),
        )
        .unwrap();
        assert_eq!(anchor.map(10_000), Some(MediaTimestamp(2_000_000_050)));
        assert_eq!(anchor.map(9_000), Some(MediaTimestamp(1_000_000_050)));
        assert_eq!(anchor.map(11_000), Some(MediaTimestamp(3_000_000_050)));
        assert_eq!(anchor.uncertainty_ns(), 1_000_050);
    }
    #[test]
    fn counter_mapping_rejects_pre_session_times_and_overflow() {
        let anchor = ClockAnchor::from_sample(
            1,
            NonZeroU64::new(1).unwrap(),
            MediaTimestamp(0),
            MediaTimestamp(0),
        )
        .unwrap();
        assert_eq!(anchor.map(0), None);
        assert_eq!(anchor.map(u64::MAX), None);
        assert!(
            ClockAnchor::from_sample(
                1,
                NonZeroU64::new(1).unwrap(),
                MediaTimestamp(2),
                MediaTimestamp(1),
            )
            .is_none()
        );
    }
    #[test]
    fn fractional_native_ticks_use_signed_integer_floor_without_accumulation() {
        let anchor = ClockAnchor::from_sample(
            3,
            NonZeroU64::new(3).unwrap(),
            MediaTimestamp(1_000_000_000),
            MediaTimestamp(1_000_000_000),
        )
        .unwrap();
        assert_eq!(anchor.map(2), Some(MediaTimestamp(666_666_666)));
        assert_eq!(anchor.map(4), Some(MediaTimestamp(1_333_333_333)));
        assert_eq!(
            anchor.map(3 + 3 * 3600),
            Some(MediaTimestamp(3_601_000_000_000))
        );
    }
    #[test]
    fn one_hour_of_48k_audio_is_exact() {
        assert_eq!(
            sample_timestamp(48_000 * 3600, std::num::NonZeroU32::new(48_000).unwrap()),
            Some(MediaTimestamp(3_600_000_000_000))
        );
    }
    #[test]
    fn overflow_is_explicit() {
        assert_eq!(
            sample_timestamp(u64::MAX, std::num::NonZeroU32::new(1).unwrap()),
            None
        );
    }
    #[test]
    fn repeated_pauses_share_one_monotonic_timeline() {
        let mut map = PauseTimeline::default();
        assert_eq!(map.map(MediaTimestamp(100)), Some(MediaTimestamp(100)));
        assert_eq!(map.pause(MediaTimestamp(150)), Some(MediaTimestamp(150)));
        assert_eq!(map.map(MediaTimestamp(50_000)), Some(MediaTimestamp(150)));
        assert_eq!(
            map.resume(MediaTimestamp(50_150)),
            Some(MediaTimestamp(150))
        );
        assert_eq!(map.map(MediaTimestamp(50_160)), Some(MediaTimestamp(160)));
        assert_eq!(map.pause(MediaTimestamp(50_200)), Some(MediaTimestamp(200)));
        assert_eq!(
            map.resume(MediaTimestamp(80_200)),
            Some(MediaTimestamp(200))
        );
        assert_eq!(map.map(MediaTimestamp(80_220)), Some(MediaTimestamp(220)));
    }
}
