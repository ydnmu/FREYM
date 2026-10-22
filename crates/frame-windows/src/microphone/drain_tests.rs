// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Graceful drain through the real worker, queue and native-cleanup path.
//!
//! Only the native packet source is scripted: the streaming loop, the bounded queue,
//! the cleanup order and the worker's terminal settlement are the production ones.
use super::*;
use frame_audio::{DrainableAudioSession, PcmDrainPoll};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Two stereo frames per packet.
const PACKET_FRAMES: u32 = 2;
const PACKET_SAMPLES: usize = (PACKET_FRAMES * 2) as usize;

/// Known sample value of packet `index`. Distinct per packet, so a drained result
/// proves completeness, uniqueness and order at once.
fn value(index: u32) -> f32 {
    index as f32 + 1.0
}

/// A scripted native stream: it hands over `total` known packets and then simulates
/// the consumer's drain request arriving while the worker waits for data.
struct DrainStream {
    shared: Arc<Shared>,
    events: Rc<RefCell<Vec<&'static str>>>,
    handed_over: Cell<u32>,
    total: u32,
    error_at: Option<u32>,
}

impl CaptureStream for DrainStream {
    fn next_packet(
        &self,
        samples: &mut Vec<f32>,
        wait_ms: u32,
    ) -> Result<Option<native::PacketObservation>, RecorderError> {
        assert_eq!(wait_ms, WAIT_SLICE_MS);
        self.events.borrow_mut().push("packet");
        let index = self.handed_over.get();
        if self.error_at == Some(index) {
            return Err(source_lost("the device failed mid-stream".into()));
        }
        if index == self.total {
            self.shared.request_drain();
            return Ok(None);
        }
        self.handed_over.set(index + 1);
        samples.extend_from_slice(&[value(index); PACKET_SAMPLES]);
        Ok(Some(native::PacketObservation {
            frames: PACKET_FRAMES,
            device_position: u64::from(index) * u64::from(PACKET_FRAMES),
            qpc_100ns: 10 + u64::from(index),
            silent: false,
            discontinuity: false,
            timestamp_error: false,
        }))
    }

    fn finish(&mut self) -> Result<(), RecorderError> {
        self.events.borrow_mut().push("stop");
        Ok(())
    }
}

impl Drop for DrainStream {
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

fn scripted(
    shared: &Arc<Shared>,
    total: u32,
    error_at: Option<u32>,
) -> (DrainStream, Rc<RefCell<Vec<&'static str>>>) {
    let events = Rc::new(RefCell::new(Vec::new()));
    (
        DrainStream {
            shared: Arc::clone(shared),
            events: Rc::clone(&events),
            handed_over: Cell::new(0),
            total,
            error_at,
        },
        events,
    )
}

/// Runs the streaming loop, the native cleanup and the worker's own settlement,
/// exactly as the spawned worker does.
fn run_worker(shared: &Arc<Shared>, stream: DrainStream) -> Result<(), RecorderError> {
    let outcome = run_started(stream, |stream| {
        stream_packets(stream, "test", shared, format(), anchor())
    });
    settle_worker(shared, outcome.clone());
    outcome
}

fn session(shared: Arc<Shared>) -> WindowsMicrophoneSession {
    WindowsMicrophoneSession {
        shared,
        worker: None,
        source: AudioSourceId("test".to_string()),
        stopped: false,
        stop_result: None,
    }
}

fn known_block(index: u32) -> PcmBlock {
    PcmBlock {
        source_id: AudioSourceId("test".to_string()),
        timestamp: frame_clock::MediaTimestamp(1_000 + u64::from(index) * 100),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
        channels: NonZeroU16::new(2).unwrap(),
        samples: vec![value(index); PACKET_SAMPLES],
    }
}

/// Takes at most one block through either poll method.
fn take_one(session: &mut WindowsMicrophoneSession, drain_poll: bool) -> Option<PcmBlock> {
    if drain_poll {
        match session.poll_drain().unwrap() {
            PcmDrainPoll::Block(block) => Some(block),
            PcmDrainPoll::Pending | PcmDrainPoll::Drained => None,
        }
    } else {
        session.try_pcm().unwrap()
    }
}

/// The sticky error of a drain poll that must fail. Neither `PcmDrainPoll` nor
/// `PcmBlock` is `Debug`, so the error is taken without formatting the success value.
fn drain_error(session: &mut WindowsMicrophoneSession) -> RecorderError {
    session.poll_drain().err().expect("a sticky drain failure")
}

fn pcm_error(session: &mut WindowsMicrophoneSession) -> RecorderError {
    session.try_pcm().err().expect("a sticky PCM failure")
}

#[test]
fn a_requested_drain_returns_every_published_block_exactly_once_and_then_reports_drained() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let (stream, events) = scripted(&shared, 3, None);
    assert!(run_worker(&shared, stream).is_ok());
    // Three packets were published, the fourth acquisition observed the drain request,
    // and the native stream was stopped and dropped before the outcome was settled.
    let mut expected = vec!["packet"; 4];
    expected.extend_from_slice(&["stop", "drop"]);
    assert_eq!(&*events.borrow(), &expected);
    // A graceful drain keeps the published PCM; nothing was discarded.
    assert_eq!(
        shared.diagnostics.discarded_bytes.load(Ordering::Acquire),
        0
    );
    assert!(shared.queue.lock().unwrap().closed);

    let mut session = session(Arc::clone(&shared));
    let mut drained = Vec::new();
    // Alternating the ordinary poll and the drain poll must not return a block twice.
    for step in 0..3 {
        drained.push(take_one(&mut session, step % 2 == 0).expect("a published block"));
    }
    for (index, block) in drained.iter().enumerate() {
        assert_eq!(block.source_id, AudioSourceId("test".to_string()));
        assert_eq!(block.sample_rate.get(), 48_000);
        assert_eq!(block.channels.get(), 2);
        assert_eq!(block.samples, vec![value(index as u32); PACKET_SAMPLES]);
        assert_eq!(
            block.timestamp,
            frame_clock::MediaTimestamp(
                1_000 + index as u64 * u64::from(PACKET_FRAMES) * 1_000_000_000 / 48_000
            )
        );
    }
    // The queue is empty and the worker finished successfully: only now is it Drained.
    assert!(take_one(&mut session, false).is_none());
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Drained
    ));
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Drained
    ));
    let queue = shared.queue.lock().unwrap();
    assert_eq!(queue.blocks.stats().depth, 0);
    assert_eq!(queue.held_bytes, 0);
    drop(queue);
    // A stop after a completed drain is successful, idempotent and keeps the record.
    assert!(session.stop().is_ok());
    assert!(session.stop().is_ok());
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Drained
    ));
}

#[test]
fn pending_is_not_drained_and_a_contended_lock_is_never_a_verdict() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let mut session = session(Arc::clone(&shared));
    // Nothing published and nothing finished: Pending, never Drained.
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Pending
    ));
    // A published block is delivered before any drain verdict.
    publish(&shared, known_block(0)).unwrap();
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Block(_)
    ));
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Pending
    ));
    // A request alone is not a completion while the worker is still running.
    session.request_drain().unwrap();
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Pending
    ));
    // A held terminal lock is transient contention, not a verdict.
    let terminal = shared.terminal.lock().unwrap();
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Pending
    ));
    drop(terminal);
    // The worker finished its drain, but a held queue lock can still hide published
    // PCM: Pending until the empty queue is actually observed.
    shared.drained();
    let queue = shared.queue.lock().unwrap();
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Pending
    ));
    drop(queue);
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Drained
    ));
}

#[test]
fn a_mid_stream_failure_is_sticky_and_never_becomes_a_drain() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let (stream, events) = scripted(&shared, 3, Some(2));
    let error = run_worker(&shared, stream).expect_err("the source failed");
    assert_eq!(error.code, ErrorCode::SourceLost);
    let mut expected = vec!["packet"; 3];
    expected.extend_from_slice(&["stop", "drop"]);
    assert_eq!(&*events.borrow(), &expected);
    let mut session = session(Arc::clone(&shared));
    // The failure wins even though a drain was requested afterwards.
    assert_eq!(
        session.request_drain().unwrap_err().code,
        ErrorCode::SourceLost
    );
    assert_eq!(drain_error(&mut session).code, ErrorCode::SourceLost);
    // The existing failure cleanup discarded the blocks published before the error,
    // and the sticky error is still reported by stop.
    assert!(shared.diagnostics.discarded_bytes.load(Ordering::Acquire) > 0);
    assert_eq!(shared.queue.lock().unwrap().blocks.stats().depth, 0);
    assert_eq!(session.stop().unwrap_err().code, ErrorCode::SourceLost);
    // The sticky failure still wins over any drain verdict.
    assert_eq!(drain_error(&mut session).code, ErrorCode::SourceLost);
}

#[test]
fn a_failure_precedes_buffered_pcm_in_both_poll_methods() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    publish(&shared, known_block(0)).unwrap();
    *shared
        .terminal
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) =
        Some(Terminal::Failed(source_lost("injected failure".into())));
    let mut session = session(Arc::clone(&shared));
    assert_eq!(drain_error(&mut session).code, ErrorCode::SourceLost);
    assert_eq!(pcm_error(&mut session).code, ErrorCode::SourceLost);
    // The error wins, so the queued block was never handed to a consumer.
    assert_eq!(shared.queue.lock().unwrap().blocks.stats().depth, 1);
}

#[test]
fn an_aborted_drain_joins_the_worker_and_keeps_the_explicit_discard() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    publish(&shared, known_block(0)).unwrap();
    publish(&shared, known_block(1)).unwrap();
    let worker_shared = Arc::clone(&shared);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        while !worker_shared.capture_should_end() {
            std::thread::sleep(Duration::from_millis(1));
        }
        // A native teardown slower than the join: a stop that returned without
        // joining its worker could not observe the settlement below.
        std::thread::sleep(Duration::from_millis(20));
        settle_worker(&worker_shared, Ok(()));
    });
    ready_rx.recv().unwrap();
    let mut session = session(Arc::clone(&shared));
    session.worker = Some(worker);

    session.request_drain().unwrap();
    session.stop().unwrap();
    assert!(
        shared.worker_finished.load(Ordering::Acquire),
        "stop must join the worker"
    );
    // Aborting before the drain was consumed keeps the existing explicit discard.
    assert!(shared.diagnostics.discarded_bytes.load(Ordering::Acquire) > 0);
    assert_eq!(shared.queue.lock().unwrap().blocks.stats().depth, 0);
    // An aborted source cannot make progress: both drain operations fail explicitly.
    assert_eq!(drain_error(&mut session).code, ErrorCode::InvalidTransition);
    assert_eq!(
        session.request_drain().unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert!(session.try_pcm().unwrap().is_none());
    assert!(session.stop().is_ok());
}

#[test]
fn a_stop_after_a_fully_consumed_drain_leaves_nothing_behind() {
    let shared = Arc::new(Shared::new(AudioCancellation::default()));
    let (stream, _events) = scripted(&shared, 1, None);
    assert!(run_worker(&shared, stream).is_ok());
    let mut session = session(Arc::clone(&shared));
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Block(_)
    ));
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Drained
    ));
    // The stop discards an already-empty queue, so the completed drain still stands.
    session.stop().unwrap();
    assert!(session.try_pcm().unwrap().is_none());
    assert!(matches!(
        session.poll_drain().unwrap(),
        PcmDrainPoll::Drained
    ));
    assert_eq!(shared.queue.lock().unwrap().blocks.stats().depth, 0);
}
