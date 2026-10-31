//! Real Windows display enumeration tests (FRAME-0101).
//!
//! These tests query the local desktop through the production
//! `DisplayBackend::enumerate_displays` path and assert platform-independent
//! invariants, never a specific monitor model. DPI context handling is verified
//! separately by the unit tests inside the reviewed FFI module, because the crate
//! lint policy keeps `unsafe` out of every other target.
//!
//! Tests that require hardware report an explicit skip when this session has no
//! display. A skipped run is not hardware evidence and must never be recorded as a
//! pass. Keep hardware validation separate from headless checks.
#![cfg(target_os = "windows")]

use frame_capture::DisplayBackend;
use frame_contracts::{AudioRoute, AudioSourceKind, DisplayInfo, ErrorCode};
use frame_windows::PlatformBackend;

/// Enumerates the attached displays, or reports that this session has none.
fn attached_displays_or_report_skip() -> Option<Vec<DisplayInfo>> {
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

#[test]
fn reports_attached_displays_with_physical_geometry_and_scale() {
    let Some(displays) = attached_displays_or_report_skip() else {
        return;
    };

    for display in &displays {
        assert!(display.width > 0, "width for {}", display.id.0);
        assert!(display.height > 0, "height for {}", display.id.0);
        // Windows display scaling is documented as 100% to 500%.
        assert!(
            (1.0..=5.0).contains(&display.scale_factor),
            "scale factor {} for {}",
            display.scale_factor,
            display.id.0
        );
        assert!(!display.name.is_empty(), "name for {}", display.id.0);
        // FRAME-0102 reports the nominal mode rate without integer-Hz rounding.
        // It is not a capture/encode measurement; accept every finite positive
        // value without inventing a supported-mode range.
        if let Some(hz) = display.refresh_rate_hz {
            assert!(
                hz.is_finite() && hz > 0.0,
                "invalid refresh rate {hz} for {}",
                display.id.0
            );
        }
        assert!(
            !display.hdr_available,
            "no HDR capability query exists, so {} must stay conservative",
            display.id.0
        );
    }
}

#[test]
fn reports_exactly_one_primary_display_at_the_virtual_origin() {
    let Some(displays) = attached_displays_or_report_skip() else {
        return;
    };

    let primaries: Vec<&DisplayInfo> = displays.iter().filter(|d| d.is_primary).collect();
    assert_eq!(primaries.len(), 1, "exactly one display must be primary");
    assert_eq!(
        (primaries[0].x, primaries[0].y),
        (0, 0),
        "Windows places the primary display at the virtual-screen origin"
    );
    assert_eq!(
        displays[0].id, primaries[0].id,
        "the primary display must be ordered first"
    );
}

#[test]
fn identity_is_stable_and_never_a_session_adapter_name() {
    let Some(displays) = attached_displays_or_report_skip() else {
        return;
    };

    let mut ids: Vec<&str> = displays.iter().map(|d| d.id.0.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        displays.len(),
        "display identities must be unique"
    );

    for display in &displays {
        assert!(
            !display.id.0.starts_with("\\\\.\\DISPLAY"),
            "session adapter name must not be durable identity: {}",
            display.id.0
        );
        assert!(
            !display.id.0.is_empty(),
            "display identity must not be empty"
        );
    }
}

#[test]
fn repeated_enumeration_returns_identical_identity_and_order() {
    let Some(first) = attached_displays_or_report_skip() else {
        return;
    };
    let second = PlatformBackend
        .enumerate_displays()
        .expect("repeat enumeration must not fail");
    assert_eq!(
        first, second,
        "identity, geometry and order must not change between calls"
    );
}

#[test]
fn enumeration_is_thread_agnostic() {
    let Some(expected) = attached_displays_or_report_skip() else {
        return;
    };

    let handles: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(|| {
                PlatformBackend
                    .enumerate_displays()
                    .expect("enumeration must not fail on a worker thread")
            })
        })
        .collect();

    for handle in handles {
        let observed = handle.join().expect("worker thread must not panic");
        assert_eq!(
            observed, expected,
            "worker threads must report identical displays"
        );
    }
}

#[test]
fn window_audio_and_encoder_discovery_report_current_native_capabilities() {
    use frame_audio::AudioBackend;
    use frame_encode::EncoderProbe;

    let backend = PlatformBackend;
    let windows = backend
        .enumerate_windows()
        .expect("native window discovery");
    let mut ids = std::collections::HashSet::new();
    for window in windows {
        assert!(window.id.0.starts_with("window:hwnd="));
        assert!(!window.title.is_empty());
        assert!(window.process_id.is_some_and(|pid| pid > 0));
        assert!(ids.insert(window.id.0), "window identities must be unique");
    }
    // FRAME-0105 implements real microphone enumeration, so this is no longer
    // NotImplemented: an empty list is a valid answer for a machine with no active
    // capture endpoint, and every returned source must be a real Windows microphone
    // identity with the canonical kind and route.
    let sources = backend
        .enumerate_audio_sources()
        .expect("microphone enumeration must be implemented on Windows");
    for source in &sources {
        assert_eq!(source.route, AudioRoute::Ignore);
        match source.kind {
            AudioSourceKind::Microphone => {
                assert!(source.id.0.starts_with("wasapi-mic:"));
                assert!(source.process_id.is_none() && source.app_identifier.is_none());
            }
            AudioSourceKind::Application => {
                assert!(source.id.0.starts_with("wasapi-session:"));
                assert!(source.process_id.is_none_or(|pid| pid > 0));
            }
            _ => panic!("unexpected source kind in endpoint/session discovery"),
        }
    }
    let mut sorted = sources.clone();
    sorted.sort_by(|left, right| left.id.cmp(&right.id));
    assert_eq!(
        sorted, sources,
        "enumeration must be deterministically ordered"
    );
    // FRAME-0104 implements real hardware qualification, so the encoder capability is no
    // longer NotImplemented. Only a missing or too-old driver may skip; every other
    // failure is a defect, and every returned capability must be real verified hardware.
    match backend.probe() {
        Ok(capabilities) => {
            for capability in &capabilities {
                assert!(
                    capability.hardware,
                    "vendor-identified-only candidates are not returned"
                );
                assert!(matches!(capability.codec.as_str(), "h264" | "hevc" | "av1"));
                assert!(!capability.id.is_empty());
            }
            if capabilities.is_empty() {
                eprintln!(
                    "SKIP (not hardware evidence): this session qualified no hardware encoder."
                );
            }
        }
        Err(error) if error.code == ErrorCode::EncoderUnavailable => {
            eprintln!("SKIP (not hardware evidence): {}.", error.message);
        }
        Err(error) => panic!("encoder qualification must not fail unexpectedly: {error}"),
    }
}
