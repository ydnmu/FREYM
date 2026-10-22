// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Integration reproducers: no microphone or native stream is opened.
use super::*;

fn shared() -> Arc<Shared> {
    Arc::new(Shared {
        queue: Mutex::new(QueueState::default()),
        terminal: Mutex::new(None),
        diagnostics: Arc::new(SharedDiagnostics::default()),
        native: Mutex::new(None),
        cancellation: AudioCancellation::default(),
        stop: AtomicBool::new(false),
        drain: AtomicBool::new(false),
        worker_finished: AtomicBool::new(false),
        continuity_released: AtomicBool::new(false),
    })
}

fn session(shared: Arc<Shared>) -> WindowsMicrophoneSession {
    WindowsMicrophoneSession {
        shared,
        worker: None,
        source: source_id("review"),
        stopped: false,
        stop_result: None,
    }
}

fn block_with_capacity() -> PcmBlock {
    let mut samples = Vec::with_capacity(4096);
    samples.extend_from_slice(&[0.0, 0.0]);
    PcmBlock {
        source_id: source_id("review"),
        timestamp: frame_clock::MediaTimestamp(1),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
        channels: NonZeroU16::new(2).unwrap(),
        samples,
    }
}

#[test]
fn review_charges_allocated_capacity_not_length() {
    let shared = shared();
    let block = block_with_capacity();
    let expected = (block.samples.capacity() * size_of::<f32>()) as u64;
    publish(&shared, block).unwrap();
    assert_eq!(shared.queue.lock().unwrap().held_bytes, expected);
}

#[test]
fn review_failure_reconciles_discarded_queue_bytes() {
    let shared = shared();
    publish(&shared, block_with_capacity()).unwrap();
    shared.fail(source_lost("injected source loss".into()));
    let queue = shared.queue.lock().unwrap();
    assert!(queue.blocks.stats().depth == 0);
    assert_eq!(queue.held_bytes, 0);
}

#[test]
fn review_poison_is_a_sticky_error_not_empty_pcm() {
    let shared = shared();
    let worker_shared = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = worker_shared.queue.lock().unwrap();
        panic!("injected queue-owner panic");
    })
    .join();
    let mut session = session(shared);
    assert!(session.try_pcm().is_err(), "poison must not become None");
}

#[test]
fn review_try_pcm_does_not_wait_for_terminal_lock() {
    let shared = shared();
    let guard = shared.terminal.lock().unwrap();
    let worker_shared = Arc::clone(&shared);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let caller = std::thread::spawn(move || {
        let mut session = session(worker_shared);
        let result = session.try_pcm();
        let _ = tx.send(result.is_ok());
    });
    let completed_without_unlock = rx.recv_timeout(Duration::from_secs(1)).is_ok();
    drop(guard); // Always release, even if the assertion is going to fail.
    caller.join().unwrap();
    assert!(
        completed_without_unlock,
        "nonblocking PCM poll waited on terminal mutex"
    );
}

#[test]
fn review_silent_packet_obeys_the_same_size_bound() {
    let format = DeviceFormat::from_parts(
        super::format::FormatDescription {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 32,
            block_align: 8,
            valid_bits: 32,
            is_float: Some(true),
            extensible: false,
        },
        4800,
    )
    .unwrap();
    let mut out = Vec::new();
    assert!(format.convert_into(None, 4801, &mut out).is_err());
    assert!(
        out.is_empty(),
        "oversized silence must be rejected before allocation"
    );
}

#[test]
fn review_float_extensible_valid_bits_are_validated() {
    assert!(
        DeviceFormat::from_parts(
            super::format::FormatDescription {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 32,
                block_align: 8,
                valid_bits: 17,
                is_float: Some(true),
                extensible: true
            },
            480
        )
        .is_err()
    );
}

#[test]
fn review_nonfinite_test_uses_a_correctly_sized_stereo_packet() {
    let format = DeviceFormat::from_parts(
        super::format::FormatDescription {
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
    .unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&f32::NAN.to_le_bytes());
    bytes.extend_from_slice(&0.0f32.to_le_bytes());
    let error = format
        .convert_into(Some(&bytes), 1, &mut Vec::new())
        .unwrap_err();
    assert!(error.message.contains("non-finite"));
}

#[test]
fn review_pending_terminal_read_must_not_publish_buffered_pcm() {
    let shared = shared();
    publish(&shared, block_with_capacity()).unwrap();
    let mut terminal = shared.terminal.lock().unwrap();
    *terminal = Some(Terminal::Failed(source_lost(
        "injected terminal failure".into(),
    )));
    let mut session = session(Arc::clone(&shared));
    let pending = session.try_pcm();
    drop(terminal);
    assert!(
        !matches!(pending, Ok(Some(_))),
        "unknown terminal state must not allow PCM delivery"
    );
    assert!(session.try_pcm().is_err());
}

#[test]
fn review_stop_reports_queue_poison_and_releases_queued_pcm() {
    let shared = shared();
    publish(&shared, block_with_capacity()).unwrap();
    let owner = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = owner.queue.lock().unwrap();
        panic!("injected queue poison before stop");
    })
    .join();
    let mut session = session(Arc::clone(&shared));
    let outcome = session.stop();
    let queue = shared
        .queue
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    assert!(outcome.is_err(), "poison must not become a successful stop");
    assert!(
        queue.blocks.stats().depth == 0,
        "stop must release queued owned PCM"
    );
    assert_eq!(queue.held_bytes, 0);
}

#[test]
fn review_queue_and_conversion_share_one_payload_bound() {
    let shared = shared();
    let format = DeviceFormat::from_parts(
        format::FormatDescription {
            sample_rate: 192_000,
            channels: 2,
            bits_per_sample: 32,
            block_align: 8,
            valid_bits: 32,
            is_float: Some(true),
            extensible: false,
        },
        19_200,
    )
    .unwrap();
    let mut scratch = Vec::new();
    for index in 0..=MAX_QUEUED_BLOCKS {
        format.convert_into(None, 19_200, &mut scratch).unwrap();
        let bytes = (scratch.capacity() * size_of::<f32>()) as u64;
        assert!(bytes <= MAX_PCM_BLOCK_BYTES);
        let queued = shared.queue.lock().unwrap().held_bytes;
        assert!(queued + bytes <= MAX_OWNED_PCM_BYTES);
        let block = PcmBlock {
            source_id: source_id("review"),
            timestamp: frame_clock::MediaTimestamp(index as u64),
            sample_rate: format.sample_rate(),
            channels: format.channels(),
            samples: std::mem::take(&mut scratch),
        };
        let result = publish(&shared, block);
        assert_eq!(result.is_err(), index == MAX_QUEUED_BLOCKS);
    }
    assert_eq!(shared.queue.lock().unwrap().blocks.stats().dropped, 1);
    shared.close_queue();
    assert_eq!(shared.queue.lock().unwrap().held_bytes, 0);
    assert!(shared.diagnostics.discarded_bytes.load(Ordering::Acquire) > 0);
    let mut oversized = vec![0.0; MAX_PCM_BLOCK_BYTES as usize / size_of::<f32>() + 1];
    assert!(format.convert_into(None, 1, &mut oversized).is_err());
}

#[test]
fn review_finished_worker_without_terminal_is_not_an_empty_poll() {
    let shared = shared();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let owner = shared.clone();
    let worker = std::thread::spawn(move || {
        owner.worker_finished.store(true, Ordering::Release);
        sent.send(()).unwrap();
    });
    received.recv().unwrap();
    // Retain the handle exactly as a running session does. The completion marker
    // lacks terminal publication; polling must diagnose that without waiting on join.
    let mut session = session(shared);
    session.worker = Some(worker);
    assert!(
        session
            .try_pcm()
            .err()
            .unwrap()
            .message
            .contains("without publishing")
    );
}
