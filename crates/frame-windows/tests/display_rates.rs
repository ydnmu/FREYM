//! FRAME-0102 refresh-rate behaviour through the supported backend surface.
//!
//! The rational conversion and association rules are crate-private, so they are
//! covered by the unit tests inside `display_rate.rs`. This suite only exercises what
//! a consumer can observe: the optional rate on `DisplayInfo` and the human-readable
//! diagnostics report.
//!
//! Tests that need hardware report an explicit skip when this session has no display.
//! A skipped run is not hardware evidence and must never be recorded as a pass.
#![cfg(target_os = "windows")]

use frame_capture::DisplayBackend;
use frame_windows::PlatformBackend;

fn attached_displays_or_report_skip() -> Option<Vec<frame_contracts::DisplayInfo>> {
    let displays = PlatformBackend
        .enumerate_displays()
        .expect("display enumeration must not fail on a Windows desktop");
    if displays.is_empty() {
        eprintln!(
            "SKIP (not hardware evidence): this Windows session reports no attached displays."
        );
        return None;
    }
    Some(displays)
}

/// A reported rate is either absent or a finite positive number.
///
/// No upper bound is asserted: the rate is reported as Windows states it, and an
/// arbitrary acceptance range would reject valid modes instead of testing anything.
#[test]
fn reports_either_no_rate_or_a_finite_positive_rate() {
    let Some(displays) = attached_displays_or_report_skip() else {
        return;
    };

    for display in &displays {
        if let Some(hz) = display.refresh_rate_hz {
            assert!(
                hz.is_finite() && hz > 0.0,
                "reported rate {hz} for {} must be finite and positive",
                display.id.0
            );
        }
    }
}

#[test]
fn repeated_enumeration_reports_the_same_rate() {
    let Some(first) = attached_displays_or_report_skip() else {
        return;
    };
    let second = PlatformBackend
        .enumerate_displays()
        .expect("repeat enumeration must not fail");

    let first_rates: Vec<Option<f64>> = first.iter().map(|d| d.refresh_rate_hz).collect();
    let second_rates: Vec<Option<f64>> = second.iter().map(|d| d.refresh_rate_hz).collect();
    assert_eq!(
        first_rates, second_rates,
        "an unchanged desktop must report the same rates"
    );
}

/// The diagnostics report is produced from one observation and must describe every
/// display the backend sees.
///
/// The report and this enumeration are two passes, so only the stable identity is
/// compared — never a value a mode change could alter between the two calls.
#[test]
fn diagnostics_report_covers_every_enumerated_display() {
    let Some(displays) = attached_displays_or_report_skip() else {
        return;
    };

    let report = frame_windows::display_diagnostics()
        .expect("display diagnostics must be available on a Windows desktop");
    assert!(
        !report.trim().is_empty(),
        "the diagnostics report must not be empty when displays exist"
    );
    assert!(
        report.lines().count() > displays.len(),
        "the report must describe each display instead of being a single line"
    );
    for display in &displays {
        assert!(
            report.contains(&display.id.0),
            "the report must name display {}",
            display.id.0
        );
    }
    // The report states what the value is, so a placeholder report cannot pass.
    assert!(
        report.contains("not a capture rate"),
        "the report must state that the rate is not a capture rate"
    );
    assert!(
        report.contains("one enumeration pass"),
        "the report must state that its values come from one enumeration pass"
    );
}

/// Each display must get exactly one rate line, and that line must state either a
/// rate or that it is unknown. The report format is part of the documented output.
#[test]
fn diagnostics_report_has_one_rate_line_per_display() {
    let Some(displays) = attached_displays_or_report_skip() else {
        return;
    };

    let report = frame_windows::display_diagnostics()
        .expect("display diagnostics must be available on a Windows desktop");

    let rate_lines: Vec<&str> = report
        .lines()
        .filter(|line| line.trim_start().starts_with("refresh: "))
        .collect();
    assert_eq!(
        rate_lines.len(),
        displays.len(),
        "one rate line per enumerated display"
    );
    for line in rate_lines {
        assert!(
            line.contains("Hz") || line.contains("unknown"),
            "a rate line must state a rate or that it is unknown: {line}"
        );
    }
}
