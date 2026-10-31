// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Deterministic synthetic counter/timestamp inputs for `RateSampler`. No clock,
//! device or worker is opened; the caller supplies every timestamp.
use frame_metrics::{MIN_SAMPLE_WINDOW, RateSampler};
use std::time::Duration;

// --- Group 1: known rate and idle -------------------------------------------

#[test]
fn first_observation_establishes_the_baseline_and_a_known_rate_follows() {
    let mut sampler = RateSampler::new();
    assert!(!sampler.is_observed());
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    assert!(sampler.is_observed());
    let sample = sampler
        .observe(Duration::from_secs(1), 240)
        .expect("one full window");
    assert_eq!(sample.rate_hz, 240.0);
    assert_eq!(sample.events, 240);
    assert_eq!(sample.window, Duration::from_secs(1));
    assert_eq!(sample.observed_at, Duration::from_secs(1));
}

#[test]
fn rate_comes_from_the_deltas_not_the_absolute_counters() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::from_secs(10), 5_000), None);
    let sample = sampler
        .observe(Duration::from_millis(11_500), 5_150)
        .expect("window from the baseline");
    assert_eq!(sample.events, 150);
    assert_eq!(sample.window, Duration::from_millis(1_500));
    assert_eq!(sample.rate_hz, 100.0);
}

#[test]
fn a_window_with_no_events_is_a_measured_zero_and_no_observation_is_none() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 7), None);
    let sample = sampler
        .observe(Duration::from_secs(2), 7)
        .expect("idle window");
    assert_eq!(sample.rate_hz, 0.0);
    assert_eq!(sample.events, 0);
    assert_eq!(sample.window, Duration::from_secs(2));
}

#[test]
fn separate_samplers_keep_separate_stage_counters() {
    // One sampler per stage: capture deliveries, captured frames, encoded frames.
    let mut deliveries = RateSampler::new();
    let mut captured = RateSampler::new();
    let mut encoded = RateSampler::new();
    for sampler in [&mut deliveries, &mut captured, &mut encoded] {
        assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    }
    assert_eq!(
        deliveries
            .observe(Duration::from_secs(2), 360)
            .expect("delivery window")
            .rate_hz,
        180.0
    );
    assert_eq!(
        captured
            .observe(Duration::from_secs(2), 240)
            .expect("capture window")
            .rate_hz,
        120.0
    );
    assert_eq!(
        encoded
            .observe(Duration::from_secs(2), 200)
            .expect("encode window")
            .rate_hz,
        100.0
    );
}

// --- Group 2: repeated or backwards timestamps, and reset --------------------

#[test]
fn a_repeated_timestamp_emits_nothing_and_preserves_the_baseline() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    assert_eq!(sampler.observe(Duration::ZERO, 50), None);
    let sample = sampler
        .observe(Duration::from_secs(1), 300)
        .expect("window from the untouched baseline");
    assert_eq!(sample.events, 300);
    assert_eq!(sample.window, Duration::from_secs(1));
}

#[test]
fn backwards_time_rebaselines_and_returns_none() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::from_secs(5), 500), None);
    assert_eq!(sampler.observe(Duration::from_secs(4), 700), None);
    let sample = sampler
        .observe(Duration::from_secs(5), 900)
        .expect("window from the new baseline");
    assert_eq!(sample.events, 200);
    assert_eq!(sample.window, Duration::from_secs(1));
    assert_eq!(sample.observed_at, Duration::from_secs(5));
}

#[test]
fn a_counter_that_decreases_at_the_same_timestamp_is_ignored() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::from_secs(5), 500), None);
    assert_eq!(sampler.observe(Duration::from_secs(5), 400), None);
    let sample = sampler
        .observe(Duration::from_secs(6), 500)
        .expect("duplicate count did not alter the baseline");
    assert_eq!(sample.events, 0);
    assert_eq!(sample.rate_hz, 0.0);
    assert_eq!(sample.window, Duration::from_secs(1));
}

#[test]
fn reset_is_explicit_and_the_next_observation_is_a_new_baseline() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    assert_eq!(
        sampler
            .observe(Duration::from_secs(1), 100)
            .expect("first window")
            .rate_hz,
        100.0
    );
    sampler.reset();
    assert!(!sampler.is_observed());
    assert_eq!(sampler.observe(Duration::from_secs(4), 900), None);
    let sample = sampler
        .observe(Duration::from_secs(5), 1_000)
        .expect("window after the reset");
    assert_eq!(sample.events, 100);
    assert_eq!(sample.rate_hz, 100.0);
    assert_eq!(sample.observed_at, Duration::from_secs(5));
}

// --- Group 3: long windows and counter restarts ------------------------------

#[test]
fn a_long_gap_keeps_its_real_window_instead_of_one_second() {
    assert_eq!(MIN_SAMPLE_WINDOW, Duration::from_secs(1));
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    let sample = sampler
        .observe(Duration::from_secs(10), 2_400)
        .expect("long window");
    assert_eq!(sample.window, Duration::from_secs(10));
    assert_eq!(sample.events, 2_400);
    assert_eq!(sample.rate_hz, 240.0);
    assert_eq!(sample.observed_at, Duration::from_secs(10));
}

#[test]
fn sub_second_observations_accumulate_until_the_minimum_window_is_reached() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    assert_eq!(sampler.observe(Duration::from_millis(400), 96), None);
    assert_eq!(sampler.observe(Duration::from_millis(900), 216), None);
    let sample = sampler
        .observe(Duration::from_millis(1_500), 360)
        .expect("window from the baseline");
    assert_eq!(sample.window, Duration::from_millis(1_500));
    assert_eq!(sample.events, 360);
    assert_eq!(sample.rate_hz, 240.0);
}

#[test]
fn a_counter_restart_rebaselines_and_the_next_window_is_measured_from_it() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    assert_eq!(
        sampler
            .observe(Duration::from_secs(1), 240)
            .expect("first window")
            .rate_hz,
        240.0
    );
    assert_eq!(sampler.observe(Duration::from_secs(2), 5), None);
    let sample = sampler
        .observe(Duration::from_millis(3_500), 125)
        .expect("window after the restart");
    assert_eq!(sample.events, 120);
    assert_eq!(sample.window, Duration::from_millis(1_500));
    assert_eq!(sample.rate_hz, 80.0);
}

#[test]
fn every_sample_carries_the_window_it_covers_and_its_own_timestamp() {
    let mut sampler = RateSampler::new();
    assert_eq!(sampler.observe(Duration::ZERO, 0), None);
    let first = sampler
        .observe(Duration::from_secs(1), 60)
        .expect("first window");
    assert_eq!(first.window, Duration::from_secs(1));
    assert_eq!(first.observed_at, Duration::from_secs(1));
    assert_eq!(first.rate_hz, 60.0);
    let second = sampler
        .observe(Duration::from_secs(4), 120)
        .expect("second window");
    assert_eq!(second.window, Duration::from_secs(3));
    assert_eq!(second.observed_at, Duration::from_secs(4));
    assert_eq!(second.rate_hz, 20.0);
    assert_eq!(first.observed_at, Duration::from_secs(1));
}

#[test]
fn resets_within_a_sampling_window_compare_against_the_latest_observation() {
    for (elapsed, events) in [
        (Duration::from_millis(500), 130),
        (Duration::from_millis(900), 110),
    ] {
        let mut sampler = RateSampler::new();
        sampler.observe(Duration::ZERO, 100);
        sampler.observe(Duration::from_millis(800), 120);
        assert_eq!(sampler.observe(elapsed, events), None);
        assert_eq!(sampler.observe(Duration::from_secs(1), 140), None);
        let sample = sampler
            .observe(elapsed + Duration::from_secs(1), events + 100)
            .unwrap();
        assert_eq!(sample.events, 100);
        assert_eq!(sample.window, Duration::from_secs(1));
    }
}
