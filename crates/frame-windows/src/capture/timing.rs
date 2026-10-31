//! Injected-clock alignment and the native timestamp policy (FRAME-0103, ADR 0005).
//!
//! The Windows backend never invents a session zero point: it samples the native
//! QPC counter between two reads of the coordinator's `MasterClock`, keeps the
//! narrowest bracket of up to eight attempts, requires that bracket to be within one
//! millisecond, and hands the raw observations to `frame_clock::ClockAnchor`. All
//! media timestamps come from `ClockAnchor::map`, never from dequeue time, nominal
//! refresh metadata or a synthetic interval.

use frame_capture::CaptureCancellation;
use frame_clock::{ClockAnchor, MasterClock, MediaTimestamp};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use std::num::NonZeroU64;

use super::diagnostics::ClockEvidence;

/// Attempts allowed when searching for a narrow clock bracket (ADR 0005: up to eight).
pub(crate) const MAX_CLOCK_SAMPLES: u32 = 8;

/// Widest acceptable bracket between the two `MasterClock` reads (ADR 0005: 1 ms).
pub(crate) const MAX_BRACKET_NS: u64 = 1_000_000;

/// Samples QPC against the injected clock and returns the narrowest accepted bracket
/// together with the private evidence for diagnostics.
pub(crate) fn align_clock(
    clock: &MasterClock,
    cancellation: &CaptureCancellation,
    ticks_per_second: u64,
    mut sample_counter: impl FnMut() -> Result<i64, RecorderError>,
) -> Result<(ClockAnchor, ClockEvidence), RecorderError> {
    let Some(ticks_per_second) = NonZeroU64::new(ticks_per_second) else {
        return Err(clock_error(
            "QueryPerformanceFrequency reported zero, so native timestamps cannot be mapped."
                .to_string(),
        ));
    };

    let mut best: Option<(ClockAnchor, ClockEvidence)> = None;
    for attempt in 1..=MAX_CLOCK_SAMPLES {
        if cancellation.is_cancelled() {
            return Err(preparation_cancelled());
        }
        let before = clock.now();
        let ticks = sample_counter()?;
        let after = clock.now();
        let bracket_ns = after.0.saturating_sub(before.0);
        let ticks = u64::try_from(ticks).map_err(|_| {
            clock_error(
                "QueryPerformanceCounter returned a negative counter value, which cannot be mapped."
                    .to_string(),
            )
        })?;
        let Some(anchor) = ClockAnchor::from_sample(ticks, ticks_per_second, before, after) else {
            return Err(clock_error(
                "the clock bracket was inconsistent, so the native counter could not be aligned."
                    .to_string(),
            ));
        };
        let evidence = ClockEvidence {
            qpc: ticks,
            ticks_per_second: ticks_per_second.get(),
            before_ns: before.0,
            after_ns: after.0,
            bracket_ns,
            samples_taken: attempt,
            uncertainty_ns: anchor.uncertainty_ns(),
            anchor_media_ns: anchor.map(ticks).map(|time| time.0).unwrap_or(before.0),
        };
        let improved = best
            .as_ref()
            .is_none_or(|(_, best_evidence)| bracket_ns < best_evidence.bracket_ns);
        if improved {
            best = Some((anchor, evidence));
        }
        if bracket_ns <= MAX_BRACKET_NS {
            break;
        }
    }

    let Some((anchor, evidence)) = best else {
        return Err(clock_error("no clock sample could be taken.".to_string()));
    };
    if evidence.bracket_ns > MAX_BRACKET_NS {
        return Err(clock_error(format!(
            "the narrowest of {} clock samples spanned {} ns, above the {} ns limit, so native presentation timestamps cannot be placed on the session timeline within the accepted uncertainty.",
            evidence.samples_taken, evidence.bracket_ns, MAX_BRACKET_NS
        )));
    }
    Ok((anchor, evidence))
}

/// What one positive native visual update timestamp means for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationOutcome {
    /// The image predates the session epoch and is discarded; later failures are not.
    PreEpoch,
    /// The same native timestamp as the previous observation: a counted repeat.
    Repeat,
    /// A valid source update arrived behind a newer visual update already delivered.
    /// Its original PTS cannot be emitted in order, so only this stale update is dropped.
    StaleVisual,
    /// An older native timestamp than the previous observation: a source discontinuity.
    Discontinuity { previous: i64, observed: i64 },
    /// A mouse update moved backwards within its own native timestamp sequence.
    PointerDiscontinuity { previous: i64, observed: i64 },
    /// A later observation that cannot be placed on the session timeline. Explicit,
    /// because discarding it silently would hide a broken mapping.
    Unmappable { observed: i64 },
    /// A new image, mapped onto the injected session timeline.
    Accepted(MediaTimestamp),
}

/// Applies ADR 0005's timestamp rules to the separate DXGI desktop and pointer
/// event sequences, which are both measured on QPC.
pub(crate) struct TimestampPolicy {
    last_present_time: Option<i64>,
    last_mouse_update_time: Option<i64>,
    last_visual_time: Option<i64>,
    accepted_any: bool,
}

impl TimestampPolicy {
    pub(crate) fn new() -> Self {
        Self {
            last_present_time: None,
            last_mouse_update_time: None,
            last_visual_time: None,
            accepted_any: false,
        }
    }

    /// Records one DXGI update. Zero in either field means that component did not
    /// change in this acquisition. The two clocks are checked independently before
    /// their latest value is considered for the output timeline.
    pub(crate) fn observe(
        &mut self,
        anchor: &ClockAnchor,
        last_present_time: i64,
        last_mouse_update_time: i64,
    ) -> ObservationOutcome {
        self.observe_inner(anchor, last_present_time, last_mouse_update_time, false)
    }

    /// A cursor position found during `DXGI_ERROR_WAIT_TIMEOUT` is stamped with
    /// the current polling QPC. It can advance output time but is not a native
    /// `LastMouseUpdateTime`, so a later DXGI mouse event is not compared to it.
    pub(crate) fn observe_synthetic_cursor(
        &mut self,
        anchor: &ClockAnchor,
        sampled_time: i64,
    ) -> ObservationOutcome {
        self.observe_inner(anchor, 0, sampled_time, true)
    }

    fn observe_inner(
        &mut self,
        anchor: &ClockAnchor,
        last_present_time: i64,
        last_mouse_update_time: i64,
        synthetic_cursor_time: bool,
    ) -> ObservationOutcome {
        debug_assert!(last_present_time > 0 || last_mouse_update_time > 0);
        let repeated_present =
            last_present_time > 0 && self.last_present_time == Some(last_present_time);
        let repeated_mouse = !synthetic_cursor_time
            && last_mouse_update_time > 0
            && self.last_mouse_update_time == Some(last_mouse_update_time);
        if last_present_time > 0
            && let Some(previous) = self.last_present_time
            && last_present_time < previous
        {
            return ObservationOutcome::Discontinuity {
                previous,
                observed: last_present_time,
            };
        }
        if last_mouse_update_time > 0
            && !synthetic_cursor_time
            && let Some(previous) = self.last_mouse_update_time
            && last_mouse_update_time < previous
        {
            return ObservationOutcome::PointerDiscontinuity {
                previous,
                observed: last_mouse_update_time,
            };
        }
        if last_present_time > 0 {
            self.last_present_time = Some(last_present_time);
        }
        if last_mouse_update_time > 0 && !synthetic_cursor_time {
            self.last_mouse_update_time = Some(last_mouse_update_time);
        }
        if (repeated_present && (last_mouse_update_time <= 0 || repeated_mouse))
            || (repeated_mouse && last_present_time <= 0)
        {
            return ObservationOutcome::Repeat;
        }

        let visual_time = last_present_time.max(last_mouse_update_time);
        if let Some(previous) = self.last_visual_time {
            if visual_time < previous {
                return ObservationOutcome::StaleVisual;
            }
            if visual_time == previous {
                return ObservationOutcome::Repeat;
            }
        }
        self.last_visual_time = Some(visual_time);

        let mapped = u64::try_from(visual_time)
            .ok()
            .and_then(|ticks| anchor.map(ticks));
        match mapped {
            Some(timestamp) => {
                self.accepted_any = true;
                ObservationOutcome::Accepted(timestamp)
            }
            // Before the first accepted image, content presented before the session
            // epoch is discarded and counted. Afterwards a mapping failure is an
            // explicit inconsistency, which the worker turns into a terminal error.
            None if !self.accepted_any => ObservationOutcome::PreEpoch,
            None => ObservationOutcome::Unmappable {
                observed: visual_time,
            },
        }
    }
}

/// Milliseconds that a cancelled preparation reports through `InvalidTransition`.
pub(crate) fn preparation_cancelled() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "Display capture preparation was cancelled before the session became ready."
            .to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new capture session when ready; no native session was left running."
            .to_string(),
    }
}

/// A clock alignment or mapping failure. It is an unexpected native condition, not a
/// lost source.
pub(crate) fn clock_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Retry the capture session; if it persists, report the Windows build and GPU."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> ClockAnchor {
        // 10 MHz counter; the sample is taken 2.0000000 s after the session epoch.
        ClockAnchor::from_sample(
            20_000_000,
            NonZeroU64::new(10_000_000).unwrap(),
            MediaTimestamp(2_000_000_000),
            MediaTimestamp(2_000_000_000),
        )
        .unwrap()
    }

    #[test]
    fn clock_alignment_accepts_a_narrow_bracket_and_keeps_its_evidence() {
        let clock = MasterClock::default();
        let cancellation = CaptureCancellation::default();
        // A sampler that does not sleep: the real bracket around it is far below the
        // 1 ms bound, so the first sample must be accepted.
        let (anchor, evidence) = align_clock(&clock, &cancellation, 10_000_000, || Ok(1)).unwrap();
        assert_eq!(evidence.samples_taken, 1);
        assert!(evidence.bracket_ns <= MAX_BRACKET_NS);
        assert!(evidence.after_ns >= evidence.before_ns);
        assert_eq!(evidence.ticks_per_second, 10_000_000);
        assert!(anchor.uncertainty_ns() >= evidence.bracket_ns.div_ceil(2));
    }

    #[test]
    fn an_over_wide_bracket_is_rejected_instead_of_guessed() {
        let clock = MasterClock::default();
        let cancellation = CaptureCancellation::default();
        let error = align_clock(&clock, &cancellation, 10_000_000, || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            Ok(1)
        })
        .expect_err("a bracket wider than 1 ms must not be accepted");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("ns limit"));
        assert_eq!(error.recovery, RecoveryStatus::NoRecording);
    }

    #[test]
    fn a_cancelled_alignment_stops_before_sampling() {
        let clock = MasterClock::default();
        let cancellation = CaptureCancellation::default();
        cancellation.cancel();
        let error = align_clock(&clock, &cancellation, 10_000_000, || {
            panic!("a cancelled preparation must not sample the native counter")
        })
        .expect_err("cancellation must be reported");
        assert_eq!(error.code, ErrorCode::InvalidTransition);
    }

    #[test]
    fn zero_frequency_and_negative_counters_are_explicit() {
        let clock = MasterClock::default();
        let cancellation = CaptureCancellation::default();
        assert!(
            align_clock(&clock, &cancellation, 0, || Ok(1))
                .expect_err("a zero counter frequency is unusable")
                .message
                .contains("QueryPerformanceFrequency")
        );
        assert!(
            align_clock(&clock, &cancellation, 10_000_000, || Ok(-5))
                .expect_err("a negative counter is unusable")
                .message
                .contains("negative")
        );
    }

    #[test]
    fn timestamps_follow_the_native_presentation_sequence() {
        let anchor = anchor();
        let mut policy = TimestampPolicy::new();

        // 2.5 s after the epoch on a 10 MHz counter.
        let first = policy.observe(&anchor, 25_000_000, 0);
        assert_eq!(
            first,
            ObservationOutcome::Accepted(MediaTimestamp(2_500_000_000))
        );

        // Equal native timestamp: a counted repeat, not a new frame.
        assert_eq!(
            policy.observe(&anchor, 25_000_000, 0),
            ObservationOutcome::Repeat
        );

        // A newer observation is accepted and strictly later on the session timeline.
        let third = policy.observe(&anchor, 25_200_000, 0);
        match third {
            ObservationOutcome::Accepted(timestamp) => {
                assert_eq!(timestamp, MediaTimestamp(2_520_000_000))
            }
            other => panic!("unexpected outcome {other:?}"),
        }

        // An older native timestamp is a discontinuity, not a silently dropped frame.
        assert_eq!(
            policy.observe(&anchor, 25_100_000, 0),
            ObservationOutcome::Discontinuity {
                previous: 25_200_000,
                observed: 25_100_000,
            }
        );
    }

    #[test]
    fn pre_epoch_content_is_discarded_and_later_mapping_failures_are_explicit() {
        // An anchor taken at the session epoch: content presented before it maps to a
        // negative media time, which `ClockAnchor` refuses instead of clamping.
        let anchor = ClockAnchor::from_sample(
            20_000_000,
            NonZeroU64::new(10_000_000).unwrap(),
            MediaTimestamp(0),
            MediaTimestamp(0),
        )
        .unwrap();
        let mut policy = TimestampPolicy::new();

        // Content presented 1 s before the session epoch.
        assert_eq!(
            policy.observe(&anchor, 10_000_000, 0),
            ObservationOutcome::PreEpoch
        );
        // The same stale presentation again is a repeat, not a second discard.
        assert_eq!(
            policy.observe(&anchor, 10_000_000, 0),
            ObservationOutcome::Repeat
        );
        // A different stale presentation is still pre-epoch, never accepted.
        assert_eq!(
            policy.observe(&anchor, 12_000_000, 0),
            ObservationOutcome::PreEpoch
        );

        // Once an image is accepted, an unmappable observation is explicit rather than
        // another silent discard: this counter value would map past the u64 range.
        assert!(matches!(
            policy.observe(&anchor, 30_000_000, 0),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, i64::MAX, 0),
            ObservationOutcome::Unmappable { observed: i64::MAX }
        );
    }

    #[test]
    fn later_pointer_update_does_not_make_a_monotone_desktop_presentation_discontinuous() {
        let anchor = anchor();
        let mut policy = TimestampPolicy::new();
        let first_present = 137_102_030_000;
        let pointer = 137_102_048_392;
        let later_present = 137_102_038_831;
        assert!(matches!(
            policy.observe(&anchor, first_present, pointer),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, later_present, 0),
            ObservationOutcome::StaleVisual,
            "the image advanced but its PTS predates the delivered pointer composite"
        );
        assert!(matches!(
            policy.observe(&anchor, pointer + 1, 0),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, later_present - 1, 0),
            ObservationOutcome::Discontinuity {
                previous: pointer + 1,
                observed: later_present - 1,
            },
            "a genuine desktop backslide remains terminal"
        );
    }

    #[test]
    fn pointer_only_update_can_overtake_an_image_without_hiding_pointer_backslide() {
        let anchor = anchor();
        let mut policy = TimestampPolicy::new();
        assert!(matches!(
            policy.observe(&anchor, 0, 25_200_000),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, 25_100_000, 0),
            ObservationOutcome::StaleVisual
        );
        assert!(matches!(
            policy.observe(&anchor, 0, 25_300_000),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, 0, 25_250_000),
            ObservationOutcome::PointerDiscontinuity {
                previous: 25_300_000,
                observed: 25_250_000,
            }
        );
    }

    #[test]
    fn timeout_cursor_qpc_does_not_advance_the_native_mouse_sequence() {
        let anchor = anchor();
        let mut policy = TimestampPolicy::new();
        assert!(matches!(
            policy.observe(&anchor, 0, 25_000_000),
            ObservationOutcome::Accepted(_)
        ));
        assert!(matches!(
            policy.observe_synthetic_cursor(&anchor, 25_200_000),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, 0, 25_100_000),
            ObservationOutcome::StaleVisual,
            "the native mouse event advances its own sequence but predates the poll sample"
        );
        assert!(matches!(
            policy.observe(&anchor, 0, 25_300_000),
            ObservationOutcome::Accepted(_)
        ));
        assert_eq!(
            policy.observe(&anchor, 0, 25_250_000),
            ObservationOutcome::PointerDiscontinuity {
                previous: 25_300_000,
                observed: 25_250_000,
            }
        );
    }
}
