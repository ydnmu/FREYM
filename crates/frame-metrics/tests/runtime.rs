// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! The adapter's counter-to-stage mapping and its explicit reset, with caller-supplied
//! timestamps only: no clock, session, GPU or worker is opened.
use frame_metrics::{PipelineCounters, PipelineMetrics, PipelineObservation};
use std::time::Duration;

#[test]
fn counters_map_to_their_own_stage_and_reset_clears_the_baseline() {
    let mut metrics = PipelineMetrics::new();
    let observation = |elapsed_ms: u64,
                       updates: u64,
                       copies: u64,
                       captured: u64,
                       accepted: u64,
                       completed: Option<u64>| PipelineObservation {
        elapsed: Duration::from_millis(elapsed_ms),
        counters: PipelineCounters {
            capture_updates: updates,
            capture_copies: copies,
            captured_frames: captured,
            encoder_accepted: accepted,
            encoder_completed_frames: completed,
        },
        ready_queue: None,
        nominal_source_hz: Some(60.0),
    };

    // A first observation only establishes the baseline: every stage is None, because no
    // window has elapsed yet.
    assert!(!metrics.is_observed());
    let first = metrics.observe(observation(0, 0, 0, 0, 0, None));
    assert!(metrics.is_observed());
    assert_eq!(first.captured_fps, None);
    assert_eq!(first.encoded_fps, None);

    // The encoder's completed-frame count stays unverified (`None`), so its stage is not
    // observed while every other stage measures its own real window.
    let second = metrics.observe(observation(1_000, 120, 240, 30, 30, None));
    assert_eq!(second.observed_at, Duration::from_secs(1));
    assert_eq!(
        second.capture_update_rate.expect("update window").rate_hz,
        120.0
    );
    assert_eq!(
        second.capture_copy_rate.expect("copy window").rate_hz,
        240.0
    );
    assert_eq!(second.captured_fps.expect("capture window").rate_hz, 30.0);
    assert_eq!(
        second
            .encoder_accept_rate
            .expect("admission window")
            .rate_hz,
        30.0
    );
    assert_eq!(second.encoded_fps, None);
    assert_eq!(second.nominal_source_hz, Some(60.0));

    // Once the packet/frame equality is verified the encoded stage measures from its own
    // baseline, which only starts here.
    let third = metrics.observe(observation(2_000, 240, 480, 60, 60, Some(30)));
    assert_eq!(third.encoded_fps, None, "the encoded baseline starts here");
    let fourth = metrics.observe(observation(3_000, 360, 720, 90, 90, Some(60)));
    assert_eq!(
        fourth.encoded_fps.expect("encoded window").rate_hz,
        30.0,
        "only the verified completed frames produced a rate"
    );

    // Static time: the counters do not move across a real window, so the measured rate is
    // an observed zero (Some(0.0)) and not the `None` that means "not measured".
    let static_window = metrics.observe(observation(4_500, 360, 720, 90, 90, Some(60)));
    for (name, sample) in [
        ("updates", static_window.capture_update_rate),
        ("copies", static_window.capture_copy_rate),
        ("captured", static_window.captured_fps),
        ("accepted", static_window.encoder_accept_rate),
        ("encoded", static_window.encoded_fps),
    ] {
        let sample = sample.unwrap_or_else(|| panic!("{name} window"));
        assert_eq!(sample.events, 0, "{name}");
        assert_eq!(sample.rate_hz, 0.0, "{name}");
        assert_eq!(sample.window, Duration::from_millis(1_500), "{name}");
    }

    // The explicit reset is the session boundary: everything is unobserved again and the
    // next observation is a new baseline, never a rate across two sessions.
    metrics.reset();
    assert!(!metrics.is_observed());
    assert_eq!(
        metrics.observe(observation(5_000, 1_000, 2_000, 500, 500, Some(500))),
        frame_metrics::PipelineRates {
            observed_at: Duration::from_secs(5),
            capture_update_rate: None,
            capture_copy_rate: None,
            captured_fps: None,
            encoder_accept_rate: None,
            encoded_fps: None,
            ready_queue: None,
            nominal_source_hz: Some(60.0),
        }
    );
}
