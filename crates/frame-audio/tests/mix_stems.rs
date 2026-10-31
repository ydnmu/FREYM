// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
use frame_audio::{AlignedPcmProcessor, PcmBlock, PcmQuantum, PushOutcome, TrackKind};
use frame_clock::MediaTimestamp;
use frame_contracts::{AudioRoute, AudioRouteSelection, AudioSourceId, ErrorCode};
use std::num::{NonZeroU16, NonZeroU32};

fn route(id: &str, route: AudioRoute) -> AudioRouteSelection {
    AudioRouteSelection {
        controls: Default::default(),
        muted: false,
        source_id: AudioSourceId(id.into()),
        route,
    }
}
fn pcm(id: &str, frame: u64, channels: u16, samples: Vec<f32>) -> PcmBlock {
    PcmBlock {
        source_id: AudioSourceId(id.into()),
        timestamp: MediaTimestamp(frame * 1_000_000_000 / 48_000),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
        channels: NonZeroU16::new(channels).unwrap(),
        samples,
    }
}
fn accept(p: &mut AlignedPcmProcessor, block: PcmBlock) {
    if let Err((error, _)) = p.push(block) {
        panic!("{}", error.message);
    }
}
fn output(p: &mut AlignedPcmProcessor, now: u64) -> PcmQuantum {
    p.poll(MediaTimestamp(now)).unwrap().expect("ready quantum")
}

#[test]
fn four_routes_produce_exact_main_and_isolated_clean_samples() {
    let mut p = AlignedPcmProcessor::new(
        &[
            route("both", AudioRoute::MainMixAndSeparate),
            route("ignored", AudioRoute::Ignore),
            route("main", AudioRoute::MainMix),
            route("separate", AudioRoute::SeparateOnly),
        ],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("both", 0, 1, vec![0.25; 480]));
    accept(&mut p, pcm("main", 0, 2, [0.5, -0.5].repeat(480)));
    accept(&mut p, pcm("separate", 0, 1, vec![2.0; 480]));
    assert!(matches!(
        p.push(pcm("ignored", 0, 1, vec![f32::NAN])),
        Ok(PushOutcome::Ignored)
    ));
    assert_eq!(p.tracks()[0].kind, TrackKind::MainMix);
    assert_eq!(
        p.tracks()[1].kind,
        TrackKind::Clean(AudioSourceId("both".into()))
    );
    assert_eq!(
        p.tracks()[2].kind,
        TrackKind::Clean(AudioSourceId("separate".into()))
    );
    let q = output(&mut p, 0);
    assert_eq!(q.tracks[0].samples, [0.75, -0.25].repeat(480));
    assert_eq!(q.tracks[1].samples, vec![0.25; 960]);
    assert_eq!(q.tracks[2].samples, vec![2.0; 960]);
    assert!(q.gaps.is_empty());
    assert_eq!(q.clipped_samples, 0);
    for (index, track) in q.tracks.iter().enumerate() {
        assert_eq!(track.track_index, index);
        assert_eq!(track.timestamp, MediaTimestamp(0));
        assert_eq!(track.sample_rate.get(), 48_000);
        assert_eq!(track.channels.get(), 2);
    }
}

fn reordered(reverse: bool) -> Vec<PcmQuantum> {
    let mut routes = vec![
        route("a", AudioRoute::MainMixAndSeparate),
        route("b", AudioRoute::MainMix),
    ];
    if reverse {
        routes.reverse();
    }
    let mut p = AlignedPcmProcessor::new(&routes, MediaTimestamp(0)).unwrap();
    let a1 = pcm("a", 0, 1, vec![0.1; 240]);
    let a2 = pcm("a", 240, 1, vec![0.2; 720]);
    let b = pcm("b", 0, 2, [0.4, -0.4].repeat(960));
    if reverse {
        accept(&mut p, b);
        accept(&mut p, a1);
        accept(&mut p, a2);
    } else {
        accept(&mut p, a1);
        accept(&mut p, a2);
        accept(&mut p, b);
    }
    vec![output(&mut p, 0), output(&mut p, 0)]
}

#[test]
fn inter_source_arrival_order_and_split_packets_preserve_content() {
    let first = reordered(false);
    assert_eq!(first, reordered(true));
    assert_eq!(
        first[0].tracks[1].samples,
        [vec![0.1; 480], vec![0.2; 480]].concat()
    );
    assert_eq!(first[1].tracks[1].samples, vec![0.2; 960]);
    assert_eq!(first[1].tracks[0].timestamp, MediaTimestamp(10_000_000));
}

#[test]
fn deadline_reports_only_missing_ranges_and_rejects_late_input_unchanged() {
    let mut p = AlignedPcmProcessor::new(
        &[route("a", AudioRoute::MainMixAndSeparate)],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("a", 0, 1, vec![0.5; 240]));
    assert!(p.poll(MediaTimestamp(109_999_999)).unwrap().is_none());
    let q = output(&mut p, 110_000_000);
    assert_eq!(
        q.tracks[0].samples,
        [vec![0.5; 480], vec![0.0; 480]].concat()
    );
    assert_eq!(q.tracks[0].samples, q.tracks[1].samples);
    assert_eq!(q.gaps[0].missing_frames, 240);
    let block = pcm("a", 240, 1, vec![0.75; 240]);
    let ptr = block.samples.as_ptr();
    let (error, returned) = p.push(block).err().unwrap();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(returned.samples.as_ptr(), ptr);
    assert_eq!(returned.samples, vec![0.75; 240]);
    assert_eq!(p.next_frame(), 480);
    accept(&mut p, pcm("a", 480, 1, vec![0.25; 480]));
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.25; 960]);
    assert_eq!(p.source_counts().next().unwrap().2, 240);
}

#[test]
fn ended_source_drains_then_survivors_continue_and_ignored_source_cannot_keep_alive() {
    let mut p = AlignedPcmProcessor::new(
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::MainMix),
            route("ignored", AudioRoute::Ignore),
        ],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("a", 0, 1, vec![0.25; 480]));
    accept(&mut p, pcm("b", 0, 1, vec![0.5; 960]));
    p.end_source(&AudioSourceId("a".into())).unwrap();
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.75; 960]);
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.5; 960]);
    p.end_source(&AudioSourceId("b".into())).unwrap();
    p.end_source(&AudioSourceId("b".into())).unwrap();
    assert!(p.poll(MediaTimestamp(u64::MAX)).unwrap().is_none());
    assert!(p.push(pcm("b", 960, 1, vec![0.5; 480])).is_err());
}

#[test]
fn main_clips_after_sum_but_clean_keeps_finite_out_of_range_samples() {
    let mut p = AlignedPcmProcessor::new(
        &[
            route("a", AudioRoute::MainMixAndSeparate),
            route("b", AudioRoute::MainMix),
            route("c", AudioRoute::MainMix),
        ],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("a", 0, 2, [f32::MAX, -f32::MAX].repeat(480)));
    accept(&mut p, pcm("b", 0, 2, [f32::MAX, -f32::MAX].repeat(480)));
    accept(&mut p, pcm("c", 0, 2, [-f32::MAX, f32::MAX].repeat(480)));
    let q = output(&mut p, 0);
    assert_eq!(q.tracks[0].samples, [1.0, -1.0].repeat(480));
    assert_eq!(q.tracks[1].samples, [f32::MAX, -f32::MAX].repeat(480));
    assert_eq!(q.clipped_samples, 960);
    assert_eq!(p.clipped_samples(), 960);
}

#[test]
fn timestamp_quantization_snap_is_counted_without_accepting_real_overlap() {
    let mut p = AlignedPcmProcessor::new(&[route("a", AudioRoute::MainMix)], MediaTimestamp(1_000));
    // A nonzero origin is used independently of system time.
    let p = p.as_mut().unwrap();
    let mut first = pcm("a", 0, 1, vec![0.25; 240]);
    first.timestamp.0 += 1_000;
    accept(p, first);
    let mut second = pcm("a", 239, 1, vec![0.5; 240]);
    second.timestamp.0 += 1_000;
    accept(p, second);
    assert_eq!(p.source_counts().next().unwrap().1, 1);
    let mut overlap = pcm("a", 478, 1, vec![0.9; 480]);
    overlap.timestamp.0 += 1_000;
    assert!(p.push(overlap).is_err());
    let q = output(p, 0);
    assert_eq!(q.tracks[0].timestamp, MediaTimestamp(1_000));
    assert_eq!(
        q.tracks[0].samples,
        [vec![0.25; 480], vec![0.5; 480]].concat()
    );
}

#[test]
fn bad_inputs_return_original_allocation_without_advancing_state() {
    let mut p = AlignedPcmProcessor::new(&[route("a", AudioRoute::MainMix)], MediaTimestamp(1_000))
        .unwrap();
    for block in [
        pcm("a", 0, 1, vec![0.1; 480]), // before origin
        pcm("unknown", 480, 1, vec![0.1; 480]),
        pcm("a", 480, 2, vec![0.1; 3]),
        pcm("a", 480, 1, vec![f32::NAN]),
        pcm("a", 480, 1, vec![0.1; 4_801]),
        pcm("a", 48_000, 1, vec![0.1; 480]),
    ] {
        let ptr = block.samples.as_ptr();
        let len = block.samples.len();
        let capacity = block.samples.capacity();
        let timestamp = block.timestamp;
        // Keep before-origin case as supplied; other timestamps are already positive.
        let (_, returned) = p.push(block).expect_err("invalid input");
        assert_eq!(returned.samples.as_ptr(), ptr);
        assert_eq!(returned.samples.len(), len);
        assert_eq!(returned.samples.capacity(), capacity);
        assert_eq!(returned.timestamp, timestamp);
        assert_eq!(p.next_frame(), 0);
        drop(returned);
    }
    for (rate, channels) in [(44_100, 1), (48_000, 3)] {
        let mut b = pcm("a", 480, channels, vec![0.0; 480 * channels as usize]);
        b.sample_rate = NonZeroU32::new(rate).unwrap();
        assert_eq!(p.push(b).err().unwrap().0.code, ErrorCode::NotImplemented);
    }
}

#[test]
fn capacity_rejection_keeps_input_and_reserves_room_for_output() {
    let mut p =
        AlignedPcmProcessor::new(&[route("a", AudioRoute::MainMix)], MediaTimestamp(0)).unwrap();
    let mut samples = Vec::with_capacity(32 * 1024 * 1024 / 4);
    samples.resize(480, 0.25);
    let block = pcm("a", 0, 1, samples);
    let ptr = block.samples.as_ptr();
    let (_, returned) = p
        .push(block)
        .expect_err("allocated capacity plus output exceeds budget");
    assert_eq!(returned.samples.as_ptr(), ptr);
    drop(returned);
    for i in 0..32 {
        accept(&mut p, pcm("a", i, 1, vec![0.25]));
    }
    let block = pcm("a", 32, 1, vec![0.25]);
    assert!(p.push(block).is_err());
    let q = output(&mut p, 110_000_000);
    assert_eq!(&q.tracks[0].samples[..64], vec![0.25; 64]);
    assert_eq!(q.gaps[0].missing_frames, 448);
    accept(&mut p, pcm("a", 480, 1, vec![0.75; 480]));
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.75; 960]);
}

#[test]
fn configuration_and_empty_routes_are_bounded() {
    assert!(
        AlignedPcmProcessor::new(
            &[
                route("x", AudioRoute::MainMix),
                route("x", AudioRoute::Ignore)
            ],
            MediaTimestamp(0)
        )
        .is_err()
    );
    let many: Vec<_> = (0..40)
        .map(|i| route(&format!("a{i}"), AudioRoute::MainMix))
        .collect();
    assert!(AlignedPcmProcessor::new(&many, MediaTimestamp(0)).is_ok());
    let mut p =
        AlignedPcmProcessor::new(&[route("x", AudioRoute::Ignore)], MediaTimestamp(0)).unwrap();
    assert!(p.poll(MediaTimestamp(u64::MAX)).unwrap().is_none());
    let mut p =
        AlignedPcmProcessor::new(&[route("x", AudioRoute::MainMix)], MediaTimestamp(u64::MAX))
            .unwrap();
    assert!(p.poll(MediaTimestamp(u64::MAX)).is_err());
}

#[test]
fn fresh_input_after_an_emitted_one_frame_gap_is_not_snapped_into_the_past() {
    let mut p =
        AlignedPcmProcessor::new(&[route("a", AudioRoute::MainMix)], MediaTimestamp(0)).unwrap();
    accept(&mut p, pcm("a", 0, 1, vec![0.25; 479]));
    let first = output(&mut p, 110_000_000);
    assert_eq!(first.gaps[0].missing_frames, 1);
    assert_eq!(&first.tracks[0].samples[958..], &[0.0, 0.0]);
    accept(&mut p, pcm("a", 480, 1, vec![0.5; 480]));
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.5; 960]);
    assert_eq!(p.source_counts().next().unwrap().1, 0);
}

#[test]
fn finish_emits_the_exact_accepted_tail_without_padding() {
    let mut p = AlignedPcmProcessor::new(
        &[route("a", AudioRoute::MainMixAndSeparate)],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("a", 0, 1, vec![0.25; 600]));
    assert!(!p.is_drained());
    p.finish().unwrap();
    assert!(!p.is_drained());
    // A full quantum first; the frozen boundary is 600 frames, not 960.
    let full = output(&mut p, 0);
    assert_eq!(full.tracks[0].samples, vec![0.25; 960]);
    assert_eq!(full.tracks[1].samples, vec![0.25; 960]);
    assert_eq!(full.tracks[0].timestamp, MediaTimestamp(0));
    assert!(full.gaps.is_empty());
    assert_eq!(full.clipped_samples, 0);
    assert!(!p.is_drained());
    // 480 frames were already emitted, so this tail does not cover a full quantum
    // and would wait for a deadline before `finish`; after it, `now` is unused.
    let tail = output(&mut p, 0);
    for (index, track) in tail.tracks.iter().enumerate() {
        assert_eq!(track.track_index, index);
        assert_eq!(track.samples.len(), 240);
        assert_eq!(track.samples, vec![0.25; 240]);
        assert_eq!(track.timestamp, MediaTimestamp(10_000_000));
        assert_eq!(track.channels.get(), 2);
    }
    assert!(tail.gaps.is_empty());
    assert_eq!(p.next_frame(), 600);
    assert!(p.is_drained());
    assert!(p.poll(MediaTimestamp(0)).unwrap().is_none());
}

#[test]
fn finish_zero_fills_interior_gaps_and_leaves_earlier_source_ends_absent() {
    let mut p = AlignedPcmProcessor::new(
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::MainMixAndSeparate),
        ],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("a", 0, 1, vec![0.5; 480]));
    accept(&mut p, pcm("b", 0, 1, vec![0.25; 120]));
    // Source b resumes after a real 360-frame dropout.
    accept(&mut p, pcm("b", 480, 1, vec![0.25; 120]));
    // Source a is declared absent past its own end, so it is not a tail gap.
    p.end_source(&AudioSourceId("a".into())).unwrap();
    p.finish().unwrap();

    let first = output(&mut p, 0);
    assert_eq!(
        first.tracks[0].samples,
        [vec![0.75; 240], vec![0.5; 720]].concat()
    );
    assert_eq!(
        first.tracks[1].samples,
        [vec![0.25; 240], vec![0.0; 720]].concat()
    );
    assert_eq!(first.tracks[0].samples.len(), first.tracks[1].samples.len());
    assert_eq!(first.gaps.len(), 1);
    assert_eq!(first.gaps[0].source_id, AudioSourceId("b".into()));
    assert_eq!(first.gaps[0].missing_frames, 360);

    // The longest accepted source end sets the tail; nothing is padded past it.
    let second = output(&mut p, 0);
    assert_eq!(second.tracks[0].samples, vec![0.25; 240]);
    assert_eq!(second.tracks[1].samples, vec![0.25; 240]);
    assert_eq!(second.tracks[0].timestamp, MediaTimestamp(10_000_000));
    assert!(second.gaps.is_empty());
    assert_eq!(p.next_frame(), 600);
    assert!(p.is_drained());
    assert!(p.poll(MediaTimestamp(0)).unwrap().is_none());
    let gaps: Vec<_> = p
        .source_counts()
        .map(|(id, _, gaps)| (id.clone(), gaps))
        .collect();
    assert_eq!(
        gaps,
        [
            (AudioSourceId("a".into()), 0),
            (AudioSourceId("b".into()), 360)
        ]
    );
}

#[test]
fn finish_is_idempotent_and_rejects_later_input_unchanged() {
    // No contributing source at all: finish drains immediately.
    let mut empty =
        AlignedPcmProcessor::new(&[route("x", AudioRoute::Ignore)], MediaTimestamp(0)).unwrap();
    assert!(!empty.is_drained());
    empty.finish().unwrap();
    empty.finish().unwrap();
    assert!(empty.is_drained());
    assert!(empty.poll(MediaTimestamp(0)).unwrap().is_none());

    // A contributing route that never received a block finishes the same way.
    let mut silent =
        AlignedPcmProcessor::new(&[route("a", AudioRoute::MainMix)], MediaTimestamp(0)).unwrap();
    silent.finish().unwrap();
    assert!(silent.is_drained());
    assert!(silent.poll(MediaTimestamp(u64::MAX)).unwrap().is_none());

    let mut p = AlignedPcmProcessor::new(
        &[
            route("a", AudioRoute::MainMix),
            route("ignored", AudioRoute::Ignore),
        ],
        MediaTimestamp(0),
    )
    .unwrap();
    accept(&mut p, pcm("a", 0, 1, vec![0.5; 600]));
    p.finish().unwrap();
    p.finish().unwrap();
    assert!(!p.is_drained());
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.5; 960]);
    // A repeated finish during the drain must not extend the frozen boundary.
    p.finish().unwrap();
    assert_eq!(output(&mut p, 0).tracks[0].samples, vec![0.5; 240]);
    assert!(p.is_drained());
    assert!(p.poll(MediaTimestamp(0)).unwrap().is_none());
    assert_eq!(p.next_frame(), 600);

    for block in [
        pcm("a", 600, 1, vec![0.5; 480]),
        pcm("ignored", 0, 1, vec![f32::NAN]),
        pcm("unknown", 0, 1, vec![0.5; 480]),
    ] {
        let ptr = block.samples.as_ptr();
        let len = block.samples.len();
        let (error, returned) = p.push(block).expect_err("admission is closed");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(returned.samples.as_ptr(), ptr);
        assert_eq!(returned.samples.len(), len);
        assert_eq!(p.next_frame(), 600);
    }
}
