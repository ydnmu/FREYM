//! Pure source-paced frame admission (FRAME-0207).
//!
//! A frame-rate selection never creates media. It only decides which **real** captured
//! frames continue to the encoder:
//!
//! - [`FrameAdmitter::auto`] passes every real frame and imposes no rate ceiling of its
//!   own, so a source faster than 480 FPS is never capped here;
//! - [`FrameAdmitter::fixed`] admits real frames using an exact rational time budget,
//!   replenished by elapsed source time, with bounded presentation jitter.
//!   One frame of time credit tolerates presentation jitter/refresh quantization;
//!   an early matching-rate frame must not be discarded. The long-term rate stays
//!   bounded by the requested rate. Unused credit is capped after a real frame
//!   consumes its cost, so delayed delivery does not erase its fractional phase.
//!   A skip is a policy
//!   decision and is counted as one; it is never capture loss, it never moves the admitted
//!   timestamp, and no frame is ever duplicated to reach the requested rate. A request
//!   above the source's own pace therefore admits every frame instead of inventing more.
//!
//! The helper is pure: it reads timestamps and returns a decision. It owns no frame, no
//! clock, no queue and no capture resource, so it can be exercised without a GPU. The
//! caller keeps owning every frame it offered.

use frame_clock::MediaTimestamp;
use frame_contracts::RecorderError;
use std::num::NonZeroU32;

/// What the policy decided about one real captured frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAdmission {
    /// The frame continues to the encoder with its own timestamp unchanged.
    Pass,
    /// The frame is left out by the frame-rate policy. It is not a lost frame.
    SkipByPolicy,
}

/// The bounded counters of one admission policy.
///
/// `frames_skipped_by_policy` counts decisions, not failures: a frame it skipped was
/// delivered by capture and deliberately not encoded. Capture loss is measured elsewhere
/// and is never folded into this number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionCounters {
    pub frames_seen: u64,
    pub frames_passed: u64,
    pub frames_skipped_by_policy: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Every real frame is passed.
    SourcePaced,
    /// Real frames consume an exact rational source-time budget.
    Fixed { num: NonZeroU32, den: NonZeroU32 },
}

/// The admission policy of one recording session.
///
/// It is created before capture starts and is then only fed timestamps, so the rate of a
/// running session cannot change: the plan a session started with stays the plan it
/// encodes.
#[derive(Clone, Copy, Debug)]
pub struct FrameAdmitter {
    mode: Mode,
    /// The first observed timestamp, on the unchanged source axis.
    origin_ns: Option<u64>,
    /// Time credit in nanoseconds * rate numerator, bounded to two frame costs
    /// after admission. This stores time only, never buffered frames.
    credit_units: u128,
    last_pts_ns: Option<u64>,
    counters: AdmissionCounters,
}

impl FrameAdmitter {
    /// Source-paced admission: every real frame passes, with no software ceiling.
    pub fn auto() -> Self {
        Self {
            mode: Mode::SourcePaced,
            origin_ns: None,
            credit_units: 0,
            last_pts_ns: None,
            counters: AdmissionCounters::default(),
        }
    }

    /// Fixed-rate admission at `num / den` frames per second.
    ///
    /// Source-time deltas replenish a bounded rational budget. Passing one real
    /// frame consumes exactly one frame cost. Initial credit permits one extra
    /// real input for refresh quantization, without changing any media timestamp.
    pub fn fixed(num: NonZeroU32, den: NonZeroU32) -> Self {
        Self {
            mode: Mode::Fixed { num, den },
            origin_ns: None,
            credit_units: 0,
            last_pts_ns: None,
            counters: AdmissionCounters::default(),
        }
    }

    /// The counters of every decision this policy has made.
    pub fn counters(&self) -> AdmissionCounters {
        self.counters
    }

    /// Decides one real captured frame.
    ///
    /// A timestamp that is not strictly later than the previous observation is reported as
    /// an error: a repeated or backwards source timestamp is a real source anomaly and is
    /// never hidden behind an admission decision.
    pub fn admit(&mut self, pts: MediaTimestamp) -> Result<FrameAdmission, RecorderError> {
        let previous_pts = self.last_pts_ns;
        if let Some(previous) = previous_pts
            && pts.0 <= previous
        {
            return Err(RecorderError::invalid(format!(
                "The capture source reported timestamp {} ns after {} ns; a repeated or backwards timestamp cannot be admitted.",
                pts.0, previous
            )));
        }
        self.last_pts_ns = Some(pts.0);
        self.counters.frames_seen += 1;

        let admission = match self.mode {
            Mode::SourcePaced => {
                self.origin_ns.get_or_insert(pts.0);
                FrameAdmission::Pass
            }
            Mode::Fixed { num, den } => match self.origin_ns {
                // The session's first frame establishes the grid's origin: it is the axis
                // the source already uses, never a synthetic zero.
                None => {
                    self.origin_ns = Some(pts.0);
                    // Two costs initially, minus this first real input's cost.
                    self.credit_units = 1_000_000_000u128 * u128::from(den.get());
                    FrameAdmission::Pass
                }
                Some(_) => {
                    let delta =
                        pts.0 - previous_pts.expect("a fixed budget has a previous source PTS");
                    let cost = 1_000_000_000u128 * u128::from(den.get());
                    let available = self.credit_units + u128::from(delta) * u128::from(num.get());
                    if available >= cost {
                        // Cap unused credit after spending this input's cost.
                        // Clamping before debit erased the fractional source-time
                        // phase at every late presentation, causing periodic drops
                        // on a matching refresh-quantized source. A long idle gap
                        // still cannot accumulate more than two spare frame costs.
                        self.credit_units = (available - cost).min(2 * cost);
                        FrameAdmission::Pass
                    } else {
                        self.credit_units = available;
                        FrameAdmission::SkipByPolicy
                    }
                }
            },
        };

        match admission {
            FrameAdmission::Pass => self.counters.frames_passed += 1,
            FrameAdmission::SkipByPolicy => self.counters.frames_skipped_by_policy += 1,
        }
        Ok(admission)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(num: u32, den: u32) -> (NonZeroU32, NonZeroU32) {
        (NonZeroU32::new(num).unwrap(), NonZeroU32::new(den).unwrap())
    }

    #[test]
    fn matching_high_rates_keep_every_real_frame_on_a_rational_grid() {
        for (n, d) in [(120, 1), (144, 1), (240, 1), (960, 1), (144_000, 1001)] {
            let (num, den) = rate(n, d);
            let mut admission = FrameAdmitter::fixed(num, den);
            for index in 0..10_000u64 {
                let pts = 73 + index * 1_000_000_000 * u64::from(d) / u64::from(n);
                assert_eq!(
                    admission.admit(MediaTimestamp(pts)).unwrap(),
                    FrameAdmission::Pass,
                    "rate {n}/{d}, frame {index}"
                );
            }
            assert_eq!(admission.counters().frames_skipped_by_policy, 0);
        }
    }

    #[test]
    fn matching_rate_presentation_jitter_does_not_discard_real_frames() {
        for n in [60, 120, 144, 180] {
            let (num, den) = rate(n, 1);
            let mut admitter = FrameAdmitter::fixed(num, den);
            let period = 1_000_000_000 / u64::from(n);
            for index in 0..2000u64 {
                // Refresh quantization can put a real presentation almost one
                // source period before its nominal deadline.
                let jitter = if index % 2 == 0 { 0 } else { period * 3 / 4 };
                let pts = 1_000_000_000 + index * 1_000_000_000 / u64::from(n) - jitter;
                assert_eq!(
                    admitter.admit(MediaTimestamp(pts)).unwrap(),
                    FrameAdmission::Pass
                );
            }
            assert_eq!(admitter.counters().frames_skipped_by_policy, 0);
        }
    }

    #[test]
    fn late_presentation_must_not_advance_two_grid_deadlines() {
        let (num, den) = rate(60, 1);
        let mut admitter = FrameAdmitter::fixed(num, den);
        assert_eq!(
            admitter.admit(MediaTimestamp(0)).unwrap(),
            FrameAdmission::Pass
        );
        for index in 1..2000u64 {
            // Crossing a nearest-slot boundary is jitter, not a missing input.
            // A late frame and its normal successor must both survive.
            let offset = if index % 2 == 1 {
                10_000_000
            } else {
                6_000_000
            };
            let pts = index * 1_000_000_000 / 60 + offset;
            assert_eq!(
                admitter.admit(MediaTimestamp(pts)).unwrap(),
                FrameAdmission::Pass,
                "real frame {index}"
            );
        }
    }

    #[test]
    fn alternating_late_and_early_presentations_keep_fractional_time_credit() {
        for fps in [60, 120, 144, 180] {
            let (num, den) = rate(fps, 1);
            let mut admitter = FrameAdmitter::fixed(num, den);
            let offsets = [0i64, 0, 750, 500, -250, -500, -750, 0];
            for index in 0..2000u64 {
                let nominal = index * 1_000_000_000 / u64::from(fps);
                let offset = offsets[index as usize % offsets.len()]
                    * (1_000_000_000 / i64::from(fps))
                    / 1000;
                let pts = (1_000_000_000 + nominal as i64 + offset) as u64;
                assert_eq!(
                    admitter.admit(MediaTimestamp(pts)).unwrap(),
                    FrameAdmission::Pass,
                    "matching {fps} FPS, real frame {index}"
                );
            }
        }
    }

    #[test]
    fn delayed_native_delivery_does_not_repeatedly_erase_unused_budget() {
        // The measured 144 FPS native timestamps spanned phase -0.216..1.302
        // periods. Reproduce that delivery jitter without manufacturing frames.
        let (num, den) = rate(144, 1);
        let mut admitter = FrameAdmitter::fixed(num, den);
        let phase = [0i64, 400, 800, 1302, 900, 500, 100, -216, 100];
        for index in 0..2000u64 {
            let pts = 1_000_000_000i64
                + (index * 1_000_000_000 / 144) as i64
                + phase[index as usize % phase.len()] * (1_000_000_000 / 144) / 1000;
            assert_eq!(
                admitter.admit(MediaTimestamp(pts as u64)).unwrap(),
                FrameAdmission::Pass
            );
        }
    }

    #[test]
    fn jitter_credit_caps_fast_sources_and_large_gaps_without_inventing_frames() {
        let (num, den) = rate(60, 1);
        let mut admitter = FrameAdmitter::fixed(num, den);
        for index in 0..1000u64 {
            admitter.admit(MediaTimestamp(index * 1_000_000)).unwrap();
        }
        assert_eq!(admitter.counters().frames_passed, 61);
        assert_eq!(admitter.counters().frames_skipped_by_policy, 939);
        // A real gap skips empty slots. At most two unused frame costs survive
        // the current input; a fourth nearly simultaneous input is refused.
        assert_eq!(
            admitter.admit(MediaTimestamp(10_000_000_000)).unwrap(),
            FrameAdmission::Pass
        );
        assert_eq!(
            admitter.admit(MediaTimestamp(10_000_000_001)).unwrap(),
            FrameAdmission::Pass
        );
        assert_eq!(
            admitter.admit(MediaTimestamp(10_000_000_002)).unwrap(),
            FrameAdmission::Pass
        );
        assert_eq!(
            admitter.admit(MediaTimestamp(10_000_000_003)).unwrap(),
            FrameAdmission::SkipByPolicy
        );
    }

    /// A manual 50 FPS request over a real 200 Hz source passes one captured frame
    /// per period after using its one-frame burst credit: every admitted timestamp is a
    /// captured one, and the frames left out are policy skips rather than loss.
    #[test]
    fn manual_rate_subsamples_real_timestamps_on_the_source_axis() {
        let (num, den) = rate(50, 1);
        let mut admitter = FrameAdmitter::fixed(num, den);
        let source_period = 5_000_000u64;
        let mut admitted = Vec::new();
        let mut skipped = 0u64;
        for index in 0..200u64 {
            match admitter
                .admit(MediaTimestamp(index * source_period))
                .unwrap()
            {
                FrameAdmission::Pass => admitted.push(index * source_period),
                FrameAdmission::SkipByPolicy => skipped += 1,
            }
        }
        // One additional real frame uses the bounded initial jitter credit.
        assert_eq!(admitted.len(), 51);
        assert_eq!(skipped, 149);
        assert_eq!(
            admitted[0], 0,
            "the first real frame establishes the origin"
        );
        assert_eq!(admitted[1], 5_000_000);
        for (index, pts) in admitted.iter().enumerate().skip(2) {
            assert_eq!(
                *pts,
                (index as u64 - 1) * 20_000_000,
                "after burst credit, real inputs are four source periods apart"
            );
        }
        let counters = admitter.counters();
        assert_eq!(counters.frames_seen, 200);
        assert_eq!(counters.frames_passed, 51);
        assert_eq!(counters.frames_skipped_by_policy, 149);
        assert_ne!(
            counters.frames_skipped_by_policy, 0,
            "a policy skip count is reported, and it is not capture loss"
        );
    }

    /// A request above the source's own pace produces no extra frame, source-paced
    /// admission has no ceiling, and an anomalous timestamp is reported rather than
    /// admitted.
    #[test]
    fn a_rate_above_the_source_creates_no_frames_and_auto_has_no_ceiling() {
        let (num, den) = rate(1_000, 1);
        let mut admitter = FrameAdmitter::fixed(num, den);
        for index in 0..200u64 {
            assert_eq!(
                admitter.admit(MediaTimestamp(index * 5_000_000)).unwrap(),
                FrameAdmission::Pass
            );
        }
        let counters = admitter.counters();
        assert_eq!(counters.frames_seen, 200);
        assert_eq!(
            counters.frames_passed, 200,
            "no duplicate frame is invented"
        );
        assert_eq!(counters.frames_skipped_by_policy, 0);

        let mut auto = FrameAdmitter::auto();
        for index in 0..960u64 {
            assert_eq!(
                auto.admit(MediaTimestamp(index * 1_041_667)).unwrap(),
                FrameAdmission::Pass
            );
        }
        assert_eq!(auto.counters().frames_passed, 960);
        assert_eq!(auto.counters().frames_skipped_by_policy, 0);

        let mut auto = FrameAdmitter::auto();
        assert!(auto.admit(MediaTimestamp(10)).is_ok());
        assert!(auto.admit(MediaTimestamp(10)).is_err());
        assert!(auto.admit(MediaTimestamp(9)).is_err());
        assert_eq!(auto.counters().frames_seen, 1);
    }
}
