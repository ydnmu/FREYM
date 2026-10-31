//! Supported public capture behaviour for the Windows backend (FRAME-0103, corrected
//! by FRAME-0103-R1).
//!
//! These tests use only the public surface: `CaptureBackend::start`,
//! `CaptureSession::try_frame`/`stop`, `WindowsCaptureSession::diagnostics` and the
//! probe helper. The request-validation tests need no GPU and always run; the capture
//! test needs a real desktop and reports an explicit SKIP line when the machine or
//! session cannot provide one. A SKIP is not hardware evidence, and an unexpected
//! terminal error, a timestamp regression or a broken reconciliation *fails* the test
//! instead of passing quietly.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use frame_capture::{
    CaptureBackend, CaptureCancellation, CaptureContext, CaptureSession, DisplayBackend,
};
use frame_clock::MasterClock;
use frame_contracts::{
    CaptureTarget, DisplayId, ErrorCode, FpsMode, GameTarget, RecorderError, Region, WindowId,
};
use frame_windows::PlatformBackend;

fn context() -> CaptureContext {
    CaptureContext {
        clock: Arc::new(MasterClock::default()),
        cancellation: CaptureCancellation::default(),
    }
}

fn absent_display() -> DisplayId {
    DisplayId("\\\\?\\DISPLAY#FRAME0103-NOT-ATTACHED#0&0&UID0#{not-a-real-interface}".to_string())
}

#[test]
fn unsupported_capture_targets_are_explicitly_not_implemented() {
    let mut backend = PlatformBackend;
    let targets = [
        CaptureTarget::Window(WindowId("not-implemented".to_string())),
        CaptureTarget::Region(Region {
            display_id: absent_display(),
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        }),
        CaptureTarget::Game(GameTarget {
            process_id: 1,
            app_identifier: None,
        }),
    ];
    for target in targets {
        let error = backend
            .start(&target, FpsMode::AutoMatchSource, context())
            .expect_err("only display capture is implemented by this proof");
        assert_eq!(error.code, ErrorCode::NotImplemented, "{target:?}");
        assert!(!error.message.is_empty());
        assert!(!error.action.is_empty());
    }
}

#[test]
fn fixed_frame_rates_are_rejected_with_the_right_code() {
    let mut backend = PlatformBackend;
    for invalid in [0.0, -1.0, -0.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let error = backend
            .start(
                &CaptureTarget::Display(absent_display()),
                FpsMode::Fixed(invalid),
                context(),
            )
            .expect_err("a nonfinite or nonpositive fixed rate is invalid");
        assert_eq!(error.code, ErrorCode::InvalidRequest, "fixed {invalid}");
    }
    // A valid fixed rate is a real request this proof does not implement yet.
    let error = backend
        .start(
            &CaptureTarget::Display(absent_display()),
            FpsMode::Fixed(60.0),
            context(),
        )
        .expect_err("a fixed rate is not implemented by this proof");
    assert_eq!(error.code, ErrorCode::NotImplemented);
}

#[test]
fn a_cancelled_context_is_rejected_before_preparation() {
    let mut backend = PlatformBackend;
    let capture_context = context();
    capture_context.cancellation.cancel();
    let error = backend
        .start(
            &CaptureTarget::Display(absent_display()),
            FpsMode::AutoMatchSource,
            capture_context,
        )
        .expect_err("a cancelled start must not produce a session");
    assert_eq!(error.code, ErrorCode::InvalidTransition);
}

#[test]
fn an_unknown_display_identity_is_explicitly_source_lost() {
    let mut backend = PlatformBackend;
    let error = backend
        .start(
            &CaptureTarget::Display(absent_display()),
            FpsMode::AutoMatchSource,
            context(),
        )
        .expect_err("an identity that is not attached must not resolve to another display");
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert!(
        error
            .message
            .contains("not among the currently attached displays")
    );
}

/// Whether a native error is a *documented* capability or session condition that this
/// machine cannot provide.
///
/// Only these two codes are skips. With the catch-all `Internal -> NotImplemented`
/// mapping removed, an unexpected native failure or an out-of-memory condition can no
/// longer masquerade as unsupported hardware and pass as a skip.
fn is_hardware_skip(error: &RecorderError) -> bool {
    matches!(
        error.code,
        ErrorCode::NotImplemented | ErrorCode::PermissionDenied
    )
}

#[test]
fn a_real_session_delivers_strictly_increasing_timestamps_and_reconciles() {
    let backend = PlatformBackend;
    let displays = match backend.enumerate_displays() {
        Ok(displays) if !displays.is_empty() => displays,
        Ok(_) => {
            eprintln!("SKIP: no attached display is available for capture");
            return;
        }
        Err(error) => {
            eprintln!("SKIP: display enumeration failed: {}", error.message);
            return;
        }
    };
    let target_display = displays
        .iter()
        .find(|display| display.is_primary)
        .unwrap_or(&displays[0]);

    let mut backend = PlatformBackend;
    let mut session = match backend.start(
        &CaptureTarget::Display(target_display.id.clone()),
        FpsMode::AutoMatchSource,
        context(),
    ) {
        Ok(session) => session,
        Err(error) if is_hardware_skip(&error) => {
            eprintln!(
                "SKIP: capture is unavailable on this machine or session: {:?} {}",
                error.code, error.message
            );
            return;
        }
        Err(error) => panic!("capture session failed to start: {error:?}"),
    };

    let mut timestamps = Vec::new();
    let mut terminal = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match session.try_frame() {
            Ok(Some(frame)) => {
                assert_eq!(frame.width, target_display.width);
                assert_eq!(frame.height, target_display.height);
                timestamps.push(frame.timestamp.0);
                // Leases are dropped immediately: this test is not a consumer stall.
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                terminal = Some(error);
                break;
            }
        }
        if timestamps.len() >= 8 {
            break;
        }
    }

    let stop = session.stop();
    let report = session
        .diagnostics()
        .expect("diagnostics must render for a started session");
    assert!(
        report.contains("not a recording"),
        "diagnostics must distinguish the proof from recording:\n{report}"
    );
    assert!(
        report.contains("terminal error: none"),
        "a successful session must record no terminal error:\n{report}"
    );
    assert!(
        report.contains("reconcile verdict: every checked identity holds"),
        "post-stop counters must reconcile, not merely print a label:\n{report}"
    );

    if let Some(error) = terminal {
        // An unexpected terminal error on a machine that supports capture is a
        // failure, never a quiet pass.
        panic!(
            "the native session ended with {:?}: {}\n{report}",
            error.code, error.message
        );
    }
    stop.expect("teardown must succeed for a session that did not fail");

    for pair in timestamps.windows(2) {
        assert!(
            pair[0] < pair[1],
            "accepted timestamps must strictly increase: {timestamps:?}"
        );
    }
    if timestamps.is_empty() {
        eprintln!(
            "SKIP: no desktop update was delivered within the window (a static desktop is not capture evidence)"
        );
        return;
    }
    println!(
        "observed {} desktop updates with strictly increasing timestamps",
        timestamps.len()
    );
}

#[test]
fn probe_rejects_out_of_range_options_before_any_native_work() {
    assert_eq!(
        frame_windows::capture_probe(&absent_display(), 0, 0, false)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        frame_windows::capture_probe(&absent_display(), 3_601, 0, false)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        frame_windows::capture_probe(&absent_display(), 5, 5_001, false)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}
