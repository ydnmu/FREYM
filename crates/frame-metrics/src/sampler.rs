// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Caller-clocked event-rate sampling for one monotonic cumulative counter.
//!
//! The caller supplies a monotonic elapsed [`Duration`] and a cumulative `u64`
//! event count; the sampler turns the real differences between two observations
//! into events per second. It owns no clock, thread, timer, allocation or history,
//! and therefore cannot hang, drift or grow on its own.
//!
//! One sampler measures one counter. Use separate instances for separate stages,
//! for example capture deliveries, audio samples and accepted encoded frames,
//! so that no stage's events can be attributed to
//! another. A sample never claims more than its own counter measures: a packet
//! count is not an encoded-frame count unless the caller proves it is.

use std::time::Duration;

/// The shortest real elapsed window a sample may be computed over. Observations
/// closer together than this only accumulate; they never emit a sample.
pub const MIN_SAMPLE_WINDOW: Duration = Duration::from_secs(1);

/// One rate measurement over a real elapsed window.
///
/// The window travels with the value so a consumer cannot present an old number
/// as a current one: `observed_at` is the caller timestamp the value belongs to
/// and `window` is how much real time it averages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateSample {
    /// Events per second across `window`, from the real counter and time deltas.
    pub rate_hz: f64,
    /// Cumulative-counter difference counted inside `window`.
    pub events: u64,
    /// Real elapsed time between the baseline observation and this one, always at
    /// least [`MIN_SAMPLE_WINDOW`]. A long gap is reported as the long window it
    /// was, never clipped to one second.
    pub window: Duration,
    /// The caller's elapsed timestamp this sample was observed at.
    pub observed_at: Duration,
}

/// The one remembered observation a sample is measured against.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    elapsed: Duration,
    events: u64,
}

/// Bounded, allocation-free, caller-clocked counter-rate sampler.
///
/// See the module documentation for the intended per-stage usage.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateSampler {
    baseline: Option<Baseline>,
    last: Option<Baseline>,
}

impl RateSampler {
    /// Start with no baseline: the first observation only establishes one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the baseline. The sampler behaves as if it had never observed
    /// anything, so the next `observe` returns `None` and measures from there.
    pub fn reset(&mut self) {
        self.baseline = None;
        self.last = None;
    }

    /// Whether any observation has established a baseline yet. `false` is "no
    /// observation", never "zero rate".
    pub fn is_observed(&self) -> bool {
        self.baseline.is_some()
    }

    /// Feed one observation: monotonic elapsed time plus the cumulative count.
    ///
    /// Returns `Some` only when a real window of at least [`MIN_SAMPLE_WINDOW`]
    /// has elapsed since the baseline. Returned `None` cases are all distinct and
    /// none of them fabricates a value:
    ///
    /// - the first observation, which only sets the baseline;
    /// - a timestamp repeated from the latest observation, which is ignored even
    ///   when its count differs and leaves the baseline untouched;
    /// - time or count decreasing from the latest observation at a different
    ///   timestamp, which replaces the baseline with this observation;
    /// - a window shorter than the minimum, which accumulates.
    ///
    /// A window of at least the minimum with no counted events is a measured
    /// `0.0` events per second, which is idle evidence rather than unknown.
    pub fn observe(&mut self, elapsed: Duration, events: u64) -> Option<RateSample> {
        let current = Baseline { elapsed, events };
        let Some(base) = self.baseline else {
            self.baseline = Some(current);
            self.last = Some(current);
            return None;
        };
        let previous = self.last.unwrap_or(base);
        // Duplicate timestamp observations are ignored, even if their count differs.
        if elapsed == previous.elapsed {
            return None;
        }
        self.last = Some(current);
        // Compare with the latest observation, including sub-window observations.
        if elapsed < previous.elapsed || events < previous.events {
            self.baseline = Some(current);
            return None;
        }
        let window = elapsed - base.elapsed;
        if window < MIN_SAMPLE_WINDOW {
            // Keep the window baseline while remembering the latest observation
            // above, so a reset within the unfinished window is still detected.
            return None;
        }
        let delta_events = events - base.events;
        self.baseline = Some(Baseline { elapsed, events });
        Some(RateSample {
            rate_hz: delta_events as f64 / window.as_secs_f64(),
            events: delta_events,
            window,
            observed_at: elapsed,
        })
    }
}
