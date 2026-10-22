// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Small aggregation adapter over the existing [`RateSampler`]: it maps the cumulative
//! counters of one *running* capture/encode session into measured stage rates, and it
//! carries the queue gauge and the hardware nominal rate beside them.
//!
//! It reads counters, never text. Nothing here parses a diagnostics report: the numbers
//! come from the sessions' own typed counters, so a change to a report's wording cannot
//! change a measured rate.
//!
//! The adapter owns one [`RateSampler`] per stage, so no stage's events can be attributed
//! to another, and it owns no clock, thread, timer, media-queue consumer or history: the
//! caller supplies a monotonic elapsed time and the cumulative counters it read, and gets
//! back a [`PipelineRates`] value by copy. Polling it is therefore bounded and
//! allocation-free, and it cannot grow or drift on its own.
//!
//! What it deliberately does **not** claim:
//!
//! - a single observation is a **live** snapshot of separate counters, not one atomic
//!   global snapshot of the pipeline;
//! - `None` is "not observed" or "could not be read without blocking", never zero. A
//!   measured zero is `Some` with `rate_hz == 0.0`, which is what an idle, static period
//!   really produced;
//! - the hardware nominal rate is passed through as its own field and is never used to
//!   compute a rate or presented as a measured one;
//! - frame losses are **not** aggregated here. The disjoint loss categories stay in the
//!   owning session's typed counters, so two overlapping categories can never be added
//!   twice into a fictitious total in this layer.

use std::time::Duration;

use crate::QueueStats;
use crate::sampler::{RateSample, RateSampler};

/// The cumulative counters one observation carries, from the sessions' own atomics.
///
/// Every field is a cumulative total since the session started; the adapter turns the
/// differences between two observations into rates. The three capture fields are the
/// capture session's own update, copy and dequeue counter; the two encoder fields are the
/// encoder session's admitted inputs and completed frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PipelineCounters {
    /// Desktop updates that carried an image (`updates_with_image`).
    pub capture_updates: u64,
    /// Completed copies into the capture session's owned pool (`copy_completions`).
    pub capture_copies: u64,
    /// Frames the capture session handed to its consumer (`dequeued`): the captured
    /// frames of this pipeline.
    pub captured_frames: u64,
    /// Inputs the encoder admitted (`accepted_inputs`).
    pub encoder_accepted: u64,
    /// Completed native frames the encoder produced.
    ///
    /// `None` means the caller could **not** verify that the delivered H.264 packet count
    /// equals the completed-frame count, so no encoded rate is counted at all. This field
    /// is never filled from a packet counter, because a packet is not a frame until that
    /// equality is established.
    pub encoder_completed_frames: Option<u64>,
}

/// One caller-clocked observation of a running pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PipelineObservation {
    /// Monotonic elapsed time from the same origin for every observation of one pipeline.
    pub elapsed: Duration,
    pub counters: PipelineCounters,
    /// The capture session's ready-queue gauge, or `None` when it could not be read
    /// without blocking or the queue was poisoned. No value is substituted for it.
    pub ready_queue: Option<QueueStats>,
    /// The hardware nominal mode rate from the capture session's own start observation, or
    /// `None` when it was unknown. Reported separately from every measured rate.
    pub nominal_source_hz: Option<f64>,
}

/// One aggregation result: the measured rate of each stage, plus the gauges.
///
/// Each stage's sample carries the real window it was measured over, so a stage that was
/// observed later than another reports its own shorter window instead of borrowing one.
/// A `None` stage has no baseline yet, or, for [`PipelineRates::encoded_fps`], the
/// caller has not verified the packet/frame equality.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipelineRates {
    /// The caller's elapsed timestamp this result belongs to.
    pub observed_at: Duration,
    /// Desktop updates with an image per second.
    pub capture_update_rate: Option<RateSample>,
    /// Completed copies per second.
    pub capture_copy_rate: Option<RateSample>,
    /// Captured frames (capture dequeues) per second.
    pub captured_fps: Option<RateSample>,
    /// Encoder admissions per second.
    pub encoder_accept_rate: Option<RateSample>,
    /// Completed encoded frames per second.
    pub encoded_fps: Option<RateSample>,
    /// The ready-queue gauge this observation carried, unchanged.
    pub ready_queue: Option<QueueStats>,
    /// The hardware nominal source rate this observation carried, unchanged.
    pub nominal_source_hz: Option<f64>,
}

/// One caller-clocked aggregator: five stage samplers, no state that can grow.
///
/// [`PipelineMetrics::reset`] is the explicit session boundary: a new capture/encode
/// session needs a reset, so the first observation after it only establishes a new
/// baseline instead of measuring a rate across two different sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct PipelineMetrics {
    capture_updates: RateSampler,
    capture_copies: RateSampler,
    captured_frames: RateSampler,
    encoder_accepted: RateSampler,
    encoder_completed_frames: RateSampler,
}

impl PipelineMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every stage baseline, as a new session requires. After this the next
    /// observation returns `None` rates and only establishes the new baseline.
    pub fn reset(&mut self) {
        self.capture_updates.reset();
        self.capture_copies.reset();
        self.captured_frames.reset();
        self.encoder_accepted.reset();
        self.encoder_completed_frames.reset();
    }

    /// Whether any stage has a baseline. `false` is "nothing observed yet", never
    /// "zero rate".
    pub fn is_observed(&self) -> bool {
        self.capture_updates.is_observed()
            || self.capture_copies.is_observed()
            || self.captured_frames.is_observed()
            || self.encoder_accepted.is_observed()
            || self.encoder_completed_frames.is_observed()
    }

    /// Feed one observation and get the rates it measured, by copy.
    pub fn observe(&mut self, observation: PipelineObservation) -> PipelineRates {
        let PipelineObservation {
            elapsed,
            counters,
            ready_queue,
            nominal_source_hz,
        } = observation;
        PipelineRates {
            observed_at: elapsed,
            capture_update_rate: self
                .capture_updates
                .observe(elapsed, counters.capture_updates),
            capture_copy_rate: self
                .capture_copies
                .observe(elapsed, counters.capture_copies),
            captured_fps: self
                .captured_frames
                .observe(elapsed, counters.captured_frames),
            encoder_accept_rate: self
                .encoder_accepted
                .observe(elapsed, counters.encoder_accepted),
            encoded_fps: match counters.encoder_completed_frames {
                Some(frames) => self.encoder_completed_frames.observe(elapsed, frames),
                // Unverified packet/frame equality: this stage is not observed at all, so
                // its baseline is left untouched instead of being compared with a packet
                // count later.
                None => None,
            },
            ready_queue,
            nominal_source_hz,
        }
    }
}
