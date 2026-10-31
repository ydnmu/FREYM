// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Inject only native operations; execute the worker's real streaming/cleanup code.
use super::*;
use std::cell::RefCell;
use std::rc::Rc;

struct TestStream {
    shared: Arc<Shared>,
    events: Rc<RefCell<Vec<&'static str>>>,
    behavior: &'static str,
    stop_fails: bool,
}

impl CaptureStream for TestStream {
    fn packet_releases_succeeded(&self) -> bool {
        self.behavior != "error"
    }
    fn next_packet(
        &self,
        samples: &mut Vec<f32>,
        wait_ms: u32,
    ) -> Result<Option<native::PacketObservation>, RecorderError> {
        assert_eq!(wait_ms, WAIT_SLICE_MS);
        self.events.borrow_mut().push("packet");
        match self.behavior {
            "panic" => panic!("injected acquisition unwind"),
            "error" => {
                return Err(native::combine(
                    source_lost("conversion failed".into()),
                    internal("ReleaseBuffer failed".into(), "retry"),
                ));
            }
            "lost" => return Err(source_lost("device lost".into())),
            "empty" => {
                self.shared.cancellation.cancel();
                return Ok(None);
            }
            _ => {}
        }
        samples.extend_from_slice(&[0.0, 0.0]);
        // One packet, then let the real worker take its cancellation exit.
        self.shared.cancellation.cancel();
        Ok(Some(native::PacketObservation {
            frames: 1,
            device_position: 0,
            qpc_100ns: 10,
            silent: true,
            discontinuity: false,
            timestamp_error: self.behavior == "timestamp",
        }))
    }

    fn finish(&mut self) -> Result<(), RecorderError> {
        self.events.borrow_mut().push("stop");
        if self.stop_fails {
            Err(internal("Stop failed".into(), "retry"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn continuity_requires_typed_buffer_stop_and_drop_proof_even_with_source_lost() {
    for (behavior, stop_fails, accepted) in [
        ("lost", false, true),
        ("lost", true, false),
        ("error", false, false),
        ("panic", false, false),
    ] {
        let shared = Arc::new(Shared::new(AudioCancellation::default()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let stream = TestStream {
            shared: Arc::clone(&shared),
            events: Rc::clone(&events),
            behavior,
            stop_fails,
        };
        let (outcome, released) = lifecycle::run_started_with_release(stream, |stream| {
            stream_packets(stream, "test", &shared, format(), anchor())
        });
        assert_eq!(&*events.borrow(), &["packet", "stop", "drop"]);
        assert_eq!(released, accepted);
        let primary = outcome.clone().unwrap_err();
        shared
            .continuity_released
            .store(released, Ordering::Release);
        settle_worker(&shared, outcome);
        let mut session = WindowsMicrophoneSession {
            shared,
            worker: None,
            source: AudioSourceId("test".into()),
            stopped: false,
            stop_result: None,
        };
        assert_eq!(session.try_pcm().err().unwrap(), primary);
        assert_eq!(
            session.stop().unwrap_err(),
            primary,
            "ordinary stop must preserve its primary error"
        );
        assert_eq!(session.stop_for_continuity().is_ok(), accepted);
    }
}

#[test]
fn continuity_refuses_join_unwind_after_an_earlier_release_flag() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    shared.continuity_released.store(true, Ordering::Release);
    settle_worker(&shared, Err(source_lost("device lost".into())));
    let mut session = WindowsMicrophoneSession {
        shared: Arc::clone(&shared),
        worker: Some(std::thread::spawn(|| panic!("injected worker join unwind"))),
        source: AudioSourceId("test".into()),
        stopped: false,
        stop_result: None,
    };
    let error = session.stop_for_continuity().unwrap_err();
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert!(error.message.contains("panicked"));
    assert!(!shared.continuity_released.load(Ordering::Acquire));
    assert_eq!(session.stop_for_continuity().unwrap_err(), error);
}

impl Drop for TestStream {
    fn drop(&mut self) {
        self.events.borrow_mut().push("drop");
    }
}

fn format() -> DeviceFormat {
    DeviceFormat::from_parts(
        format::FormatDescription {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 32,
            block_align: 8,
            valid_bits: 32,
            is_float: Some(true),
            extensible: false,
        },
        480,
    )
    .unwrap()
}

fn anchor() -> ClockAnchor {
    ClockAnchor::from_sample(
        10,
        std::num::NonZeroU64::new(10_000_000).unwrap(),
        frame_clock::MediaTimestamp(1_000),
        frame_clock::MediaTimestamp(1_000),
    )
    .unwrap()
}

#[test]
fn streaming_exits_stop_once_and_drop_before_returning() {
    for behavior in ["success", "timestamp", "error", "panic", "empty"] {
        for stop_fails in [false, true] {
            let shared = Arc::new(Shared::new(AudioCancellation::default()));
            let events = Rc::new(RefCell::new(Vec::new()));
            let stream = TestStream {
                shared: shared.clone(),
                events: events.clone(),
                behavior,
                stop_fails,
            };
            let result = run_started(stream, |stream| {
                stream_packets(stream, "test", &shared, format(), anchor())
            });
            assert_eq!(&*events.borrow(), &["packet", "stop", "drop"], "{behavior}");
            assert_eq!(
                result.is_err(),
                stop_fails || matches!(behavior, "timestamp" | "error" | "panic")
            );
            if let Err(error) = result {
                if stop_fails {
                    assert!(error.message.contains("Stop failed"));
                }
                if behavior == "error" {
                    assert_eq!(error.code, ErrorCode::SourceLost);
                    assert!(error.message.contains("conversion failed"));
                    assert!(error.message.contains("ReleaseBuffer failed"));
                }
                if behavior == "panic" {
                    assert!(error.message.contains("panicked"));
                }
            }
        }
    }
}

#[test]
fn readiness_failure_and_cancellation_stop_before_any_packet() {
    for reason in ["receiver", "cancel", "deadline"] {
        let shared = Arc::new(Shared::new(AudioCancellation::default()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let deadline = if reason == "deadline" {
            Instant::now()
        } else {
            Instant::now() + PREPARATION_WATCHDOG
        };
        if reason == "cancel" {
            shared.cancellation.cancel();
        }
        if reason == "receiver" {
            drop(receiver);
        }
        let stream = TestStream {
            shared: shared.clone(),
            events: events.clone(),
            behavior: "success",
            stop_fails: true,
        };
        let error = run_started(stream, |stream| {
            announce_ready(&shared, deadline, &sender)?;
            stream_packets(stream, "test", &shared, format(), anchor())
        })
        .unwrap_err();
        assert!(error.message.contains("Stop failed"));
        assert_eq!(&*events.borrow(), &["stop", "drop"]);
        if reason == "cancel" {
            assert_eq!(error.code, ErrorCode::InvalidTransition);
        }
    }
}

#[test]
fn cancellation_after_readiness_stops_without_acquiring() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let events = Rc::new(RefCell::new(Vec::new()));
    let stream = TestStream {
        shared: shared.clone(),
        events: events.clone(),
        behavior: "panic",
        stop_fails: false,
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    run_started(stream, |stream| {
        announce_ready(&shared, Instant::now() + PREPARATION_WATCHDOG, &sender)?;
        assert!(receiver.try_recv().unwrap().is_ok());
        shared.cancellation.cancel();
        stream_packets(stream, "test", &shared, format(), anchor())
    })
    .unwrap();
    assert_eq!(&*events.borrow(), &["stop", "drop"]);
}

#[test]
fn native_lifecycle_never_stops_unstarted_or_retries_failed_stop() {
    let calls = std::cell::Cell::new(0);
    for started in [false, true] {
        let mut lifecycle = lifecycle::CaptureLifecycle::default();
        if started {
            lifecycle.start(|| Ok(())).unwrap();
        } else {
            assert!(
                lifecycle
                    .start(|| Err(internal("Start failed".into(), "retry")))
                    .is_err()
            );
        }
        assert_eq!(lifecycle.running(), started);
        let first = lifecycle.finish(|| {
            calls.set(calls.get() + 1);
            Err(internal("Stop failed".into(), "retry"))
        });
        assert_eq!(first.is_err(), started);
        assert!(!lifecycle.running());
        lifecycle.finish(|| panic!("must not retry Stop")).unwrap();
    }
    assert_eq!(calls.get(), 1);
}

#[test]
fn controller_cancellation_preserves_failure_observed_during_join() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let worker_shared = shared.clone();
    let worker = std::thread::spawn(move || {
        worker_shared.fail(internal("Stop failed while cancelling".into(), "retry"));
    });
    let error = join_preparation_failure(worker, &shared, cancelled_start());
    assert_eq!(error.code, ErrorCode::InvalidTransition);
    assert!(error.message.contains("cancelled"));
    assert!(error.message.contains("Stop failed while cancelling"));
}

#[test]
fn queue_publication_error_also_runs_explicit_stop() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    // Force the production publish() path to hit queue contention after conversion.
    let queue_lock = shared.queue.lock().unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let stream = TestStream {
        shared: shared.clone(),
        events: events.clone(),
        behavior: "success",
        stop_fails: true,
    };
    let error = run_started(stream, |stream| {
        stream_packets(stream, "test", &shared, format(), anchor())
    })
    .unwrap_err();
    drop(queue_lock);
    assert!(error.message.contains("queue lock"));
    assert!(error.message.contains("Stop failed"));
    assert_eq!(&*events.borrow(), &["packet", "stop", "drop"]);
}

#[test]
fn poisoned_observation_log_fails_through_worker_cleanup() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let poisoned = shared.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poisoned.diagnostics.observations.lock().unwrap();
        panic!("injected observation poison");
    })
    .join();
    let events = Rc::new(RefCell::new(Vec::new()));
    let stream = TestStream {
        shared: shared.clone(),
        events: events.clone(),
        behavior: "success",
        stop_fails: true,
    };
    let error = run_started(stream, |stream| {
        stream_packets(stream, "test", &shared, format(), anchor())
    })
    .unwrap_err();
    assert!(error.message.contains("observation log was poisoned"));
    assert!(error.message.contains("Stop failed"));
    assert_eq!(&*events.borrow(), &["packet", "stop", "drop"]);
}

#[test]
fn contiguous_device_frames_use_one_qpc_anchor_despite_packet_jitter() {
    struct JitterStream {
        shared: Arc<Shared>,
        next: std::cell::Cell<usize>,
        gap: bool,
    }
    impl CaptureStream for JitterStream {
        fn next_packet(
            &self,
            samples: &mut Vec<f32>,
            _: u32,
        ) -> Result<Option<native::PacketObservation>, RecorderError> {
            let i = self.next.get();
            if i == 4 {
                self.shared.request_drain();
                return Ok(None);
            }
            self.next.set(i + 1);
            samples.resize(960, 0.25);
            Ok(Some(native::PacketObservation {
                frames: 480,
                device_position: 5000 + i as u64 * 480 + u64::from(self.gap && i == 3),
                qpc_100ns: [10, 100_310, 199_510, 299_010][i],
                silent: false,
                discontinuity: false,
                timestamp_error: false,
            }))
        }
        fn finish(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
    }
    for gap in [false, true] {
        let shared = Arc::new(Shared::new(AudioCancellation::default()));
        let stream = JitterStream {
            shared: shared.clone(),
            next: std::cell::Cell::new(0),
            gap,
        };
        let result = run_started(stream, |stream| {
            stream_packets(stream, "test", &shared, format(), anchor())
        });
        if gap {
            assert_eq!(result.unwrap_err().code, ErrorCode::SourceLost);
        } else {
            result.unwrap();
            let mut queue = shared.queue.lock().unwrap();
            for i in 0..4 {
                let block = queue.blocks.pop().unwrap();
                assert_eq!(block.timestamp.0, 1_000 + i * 10_000_000);
                assert_eq!(block.samples, vec![0.25; 960]);
            }
        }
    }
}
