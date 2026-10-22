// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! FRAME-0206 focused checks: real native formats on the accepted 48 kHz line.
//!
//! Two focused checks, both over the accepted contracts instead of a device:
//!
//! 1. a 44.1 kHz (and a 96 kHz) stereo source reaches the accepted mixer through the
//!    adapter with the right content, the exact accepted duration, a trimmed head and a
//!    real tail, and a real source discontinuity is an explicit error;
//! 2. a 48 kHz source is bypassed unchanged (samples, block boundaries and timestamps),
//!    mono is duplicated, and unsupported rates, layouts and oversized blocks are
//!    explicit errors that stay sticky.

use frame_audio::{
    AlignedPcmProcessor, AudioSession, DrainableAudioSession, NORMALIZED_RATE,
    NormalizingAudioSession, OUTPUT_BLOCK_FRAMES, PcmBlock, PcmDrainPoll,
};
use frame_clock::{MediaTimestamp, sample_timestamp};
use frame_contracts::{AudioRoute, AudioRouteSelection, AudioSourceId, ErrorCode, RecorderError};
use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroU32};

/// Native epoch of every check: deliberately not on any 48 kHz grid.
const T0: u64 = 1_700_000_000_123_456;
/// The signal every check carries: 1 kHz, amplitude 0.5.
const TONE_HZ: f64 = 1_000.0;
const TONE_AMPLITUDE: f64 = 0.5;

#[test]
fn enabled_compensation_slews_real_resampler_and_keeps_one_timestamp_grid() {
    struct Clocked {
        tick: u64,
        ppm: i64,
    }
    impl AudioSession for Clocked {
        fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
            if self.tick >= 1_400 {
                return Ok(None);
            }
            let tick = self.tick;
            self.tick += 1;
            Ok(Some(PcmBlock {
                source_id: AudioSourceId("corrected".into()),
                timestamp: MediaTimestamp(
                    T0 + (u128::from(tick * 50_000_000) * 1_000_000
                        / (1_000_000 + self.ppm) as u128) as u64,
                ),
                sample_rate: rate(48_000),
                channels: channels(2),
                samples: vec![0.25; 4800],
            }))
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
    }
    impl DrainableAudioSession for Clocked {
        fn request_drain(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
        fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
            Ok(PcmDrainPoll::Drained)
        }
    }
    for ppm in [-200, 200] {
        let mut session = NormalizingAudioSession::new(
            Clocked { tick: 0, ppm },
            AudioSourceId("corrected".into()),
        )
        .with_drift_compensation(true);
        let mut frames = 0;
        for _ in 0..20_000 {
            if let Some(block) = session.try_pcm().unwrap() {
                assert_eq!(
                    block.timestamp.0,
                    T0 + sample_timestamp(frames, rate(48_000)).unwrap().0
                );
                frames += block.samples.len() as u64 / 2;
            } else if session.counters().blocks_in == 1_400 {
                break;
            }
        }
        let health = session.compensation_health().unwrap();
        assert!(health.enabled && health.reliable);
        assert_eq!(health.applied_milli_ppm.signum(), ppm.signum());
        assert!(health.applied_milli_ppm.abs() <= 1_000_000);
        session.request_drain().unwrap();
        let mut drained = false;
        for _ in 0..100 {
            match session.poll_drain().unwrap() {
                PcmDrainPoll::Block(block) => {
                    assert_eq!(
                        block.timestamp.0,
                        T0 + sample_timestamp(frames, rate(48_000)).unwrap().0
                    );
                    frames += block.samples.len() as u64 / 2;
                }
                PcmDrainPoll::Drained => {
                    drained = true;
                    break;
                }
                PcmDrainPoll::Pending => {}
            }
        }
        assert!(drained);
        let real_duration = (70_000_000_000u128 * 1_000_000 / (1_000_000 + ppm) as u128) as i128;
        let error_ns = (frames as i128 * 1_000_000_000 / 48_000 - real_duration).abs();
        println!(
            "compensation proof: source_ppm={ppm}, applied_milli_ppm={}, output_frames={frames}, drained_residual_ns={error_ns}",
            health.applied_milli_ppm
        );
        assert!(
            error_ns < 8_000_000,
            "ppm={ppm}, residual={error_ns}, health={health:?}"
        );
    }
}

#[test]
fn raw_clock_speed_is_observed_without_changing_48k_pcm_or_timestamps() {
    struct ClockedSession(u64);
    impl AudioSession for ClockedSession {
        fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
            let tick = self.0;
            if tick > 4200 {
                return Ok(None);
            }
            self.0 += 1;
            Ok(Some(PcmBlock {
                source_id: AudioSourceId("clocked".into()),
                timestamp: MediaTimestamp(
                    T0 + (u128::from(tick * 10_000_000) * 1_000_000 / 1_000_200) as u64,
                ),
                sample_rate: rate(48_000),
                channels: channels(2),
                samples: vec![0.25; 960],
            }))
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
    }
    let mut session =
        NormalizingAudioSession::new(ClockedSession(0), AudioSourceId("clocked".into()));
    for tick in 0..=4200u64 {
        let block = session.try_pcm().unwrap().unwrap();
        assert_eq!(
            block.timestamp.0,
            T0 + (u128::from(tick * 10_000_000) * 1_000_000 / 1_000_200) as u64
        );
        assert_eq!(block.samples, vec![0.25; 960]);
    }
    let drift = session.clock_drift().unwrap();
    assert!(drift.reliable);
    assert!(drift.rate_ppm.abs_diff(200) <= 1);
    assert!(drift.residual_ns < -5_000_000);
    session.stop().unwrap();
    assert_eq!(session.clock_drift(), Some(drift));
}

fn rate(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("nonzero rate")
}

fn channels(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).expect("nonzero channels")
}

/// One source sample of the 1 kHz tone at `frame` in a source of `sample_rate`.
fn tone(frame: u64, sample_rate: u32) -> f32 {
    (TONE_AMPLITUDE
        * (2.0 * std::f64::consts::PI * TONE_HZ * frame as f64 / f64::from(sample_rate)).sin())
        as f32
}

/// The same tone on the 48 kHz grid, which is what a faithful conversion must produce.
fn tone_48k(frame: f64) -> f32 {
    (TONE_AMPLITUDE
        * (2.0 * std::f64::consts::PI * TONE_HZ * frame / f64::from(NORMALIZED_RATE)).sin())
        as f32
}

/// The accepted native block grid: the device anchor plus the exact sample conversion.
fn grid_timestamp(offset: u64, sample_rate: u32) -> MediaTimestamp {
    MediaTimestamp(T0 + sample_timestamp(offset, rate(sample_rate)).expect("grid").0)
}

fn block(
    source_id: &AudioSourceId,
    sample_rate: u32,
    channel_count: u16,
    offset: u64,
    samples: Vec<f32>,
) -> PcmBlock {
    PcmBlock {
        source_id: source_id.clone(),
        timestamp: grid_timestamp(offset, sample_rate),
        sample_rate: rate(sample_rate),
        channels: channels(channel_count),
        samples,
    }
}

/// One native block of the stereo tone: left is the tone, right is its negative, so a
/// channel swap or a downmix cannot pass unnoticed.
fn tone_block(
    source_id: &AudioSourceId,
    sample_rate: u32,
    frames: usize,
    channel_count: u16,
    offset: u64,
) -> PcmBlock {
    let mut samples = Vec::with_capacity(frames * usize::from(channel_count));
    for index in 0..frames as u64 {
        let value = tone(offset + index, sample_rate);
        samples.push(value);
        if channel_count == 2 {
            samples.push(-value);
        }
    }
    block(source_id, sample_rate, channel_count, offset, samples)
}

/// A scripted native session: the accepted session contract over a fixed list of
/// already-published blocks, so the adapter is exercised without a device.
struct ScriptedSession {
    blocks: VecDeque<PcmBlock>,
    drain_requested: bool,
}

impl ScriptedSession {
    fn new(blocks: Vec<PcmBlock>) -> Self {
        Self {
            blocks: blocks.into(),
            drain_requested: false,
        }
    }
}

impl AudioSession for ScriptedSession {
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        Ok(self.blocks.pop_front())
    }

    fn stop(&mut self) -> Result<(), RecorderError> {
        self.blocks.clear();
        Ok(())
    }
}

impl DrainableAudioSession for ScriptedSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        self.drain_requested = true;
        Ok(())
    }

    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        if !self.drain_requested {
            return Ok(PcmDrainPoll::Pending);
        }
        match self.blocks.pop_front() {
            Some(block) => Ok(PcmDrainPoll::Block(block)),
            None => Ok(PcmDrainPoll::Drained),
        }
    }
}

/// Reads the whole normalized stream: at most `live_polls` blocks on the live path, the
/// rest through the accepted drain path, so both are exercised in order.
fn read_stream(
    session: &mut NormalizingAudioSession<ScriptedSession>,
    live_polls: usize,
) -> Result<Vec<PcmBlock>, RecorderError> {
    let mut blocks = Vec::new();
    for _ in 0..live_polls {
        match session.try_pcm()? {
            Some(block) => blocks.push(block),
            None => break,
        }
    }
    session.request_drain()?;
    let mut budget = 10_000usize;
    loop {
        match session.poll_drain()? {
            PcmDrainPoll::Block(block) => blocks.push(block),
            PcmDrainPoll::Pending => {
                budget -= 1;
                assert!(budget > 0, "the accepted drain never settled");
            }
            PcmDrainPoll::Drained => break,
        }
    }
    Ok(blocks)
}

fn interleave(blocks: &[PcmBlock]) -> Vec<f32> {
    blocks
        .iter()
        .flat_map(|block| block.samples.iter().copied())
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    let sum: f64 = samples.iter().map(|value| f64::from(*value).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

#[test]
fn resampling_preserves_high_frequency_energy() {
    let source_id = AudioSourceId("sinc-bandwidth".into());
    let blocks = (0..20)
        .map(|chunk| {
            let offset = chunk * 441;
            let samples = (offset..offset + 441)
                .flat_map(|frame| {
                    let value = (0.5
                        * (2.0 * std::f64::consts::PI * 18_000.0 * frame as f64 / 44_100.0).sin())
                        as f32;
                    [value, -value]
                })
                .collect();
            block(&source_id, 44_100, 2, offset, samples)
        })
        .collect();
    let mut session = NormalizingAudioSession::new(ScriptedSession::new(blocks), source_id);
    let samples = interleave(&read_stream(&mut session, 10).expect("normalize tone"));
    assert_eq!(samples.len(), 9_600 * 2);
    let steady = &samples[480 * 2..samples.len() - 480 * 2];
    let gain = rms(steady) / (0.5 / 2.0_f64.sqrt());
    assert!(
        (0.98..=1.02).contains(&gain),
        "18 kHz amplitude gain {gain}"
    );
}

/// The exact 48 kHz frame count of an accepted source duration, as the adapter derives
/// it: `source_frames * 48_000 / rate`, rounded to the nearest whole frame.
fn expected_frames(source_frames: u64, sample_rate: u32) -> u64 {
    let numerator = u64::from(NORMALIZED_RATE / gcd(NORMALIZED_RATE, sample_rate));
    let denominator = u64::from(sample_rate / gcd(NORMALIZED_RATE, sample_rate));
    (source_frames * numerator + denominator / 2) / denominator
}

fn gcd(left: u32, right: u32) -> u32 {
    if right == 0 {
        left
    } else {
        gcd(right, left % right)
    }
}

/// Both channels of a whole normalized stream, checked against the 48 kHz tone.
fn assert_tone_content(samples: &[f32], frames: u64, source_rate: u32, tolerance: f32) {
    // Async's sinc output advances before sampling and reports integer delay.
    // Retain its <=1-sample phase offset rather than filtering the output again.
    let phase_frames = 1.0 - (64.0 * f64::from(NORMALIZED_RATE) / f64::from(source_rate)).fract();
    assert!((0.0..=1.0).contains(&phase_frames));
    let mut max_diff = 0.0f32;
    let mut worst_frame = 0;
    let mut startup_max_diff = 0.0f32;
    // The source's own abrupt end is smeared over the resampler's filter length, exactly
    // as the official whole-clip path does, so the last 10 ms is checked separately.
    for frame in 0..frames.saturating_sub(OUTPUT_BLOCK_FRAMES as u64) {
        let reference = tone_48k(frame as f64 + phase_frames);
        let diff = (samples[frame as usize * 2] - reference)
            .abs()
            .max((samples[frame as usize * 2 + 1] + reference).abs());
        // A sinc filter sees zero history before the first accepted source frame.
        // Keep its short boundary transient separately bounded; the continuing
        // stream must still meet the original tight tone tolerance.
        if frame < 128 {
            startup_max_diff = startup_max_diff.max(diff);
            continue;
        }
        if diff > max_diff {
            max_diff = diff;
            worst_frame = frame;
        }
    }
    assert!(
        max_diff <= tolerance,
        "the normalized 48 kHz content differs from the reference tone by {max_diff} at frame {worst_frame}, above {tolerance}; sample={}, reference={}, next={}",
        samples[worst_frame as usize * 2],
        tone_48k(worst_frame as f64 + phase_frames),
        samples[(worst_frame as usize + 1) * 2]
    );
    assert!(
        startup_max_diff <= 0.03,
        "the sinc startup transient is {startup_max_diff}, above 0.03"
    );
}

fn code_of(block: PcmBlock, source_id: &AudioSourceId) -> ErrorCode {
    let mut session =
        NormalizingAudioSession::new(ScriptedSession::new(vec![block]), source_id.clone());
    let Err(error) = session.try_pcm() else {
        panic!("the adapter accepted a block it must refuse");
    };
    // Sticky: the same failure is repeated instead of looking like an idle source.
    let Err(again) = session.try_pcm() else {
        panic!("the stored failure was not repeated");
    };
    assert_eq!(error.code, again.code);
    assert_eq!(error.message, again.message);
    error.code
}

#[test]
fn forty_four_one_kilohertz_reaches_the_mixer_with_the_accepted_duration_and_content() {
    const RATE: u32 = 44_100;
    // 4700 frames = 106.6 ms: a partial last resampler chunk, one flush chunk and real
    // zero padding that must not reach the output duration.
    const SOURCE_FRAMES: u64 = 4_700;
    const BLOCK_FRAMES: usize = 210;
    let source_id = AudioSourceId("wasapi-mic:frame-0206".into());

    let mut blocks = Vec::new();
    let mut offset = 0u64;
    while offset < SOURCE_FRAMES {
        let frames = (SOURCE_FRAMES - offset).min(BLOCK_FRAMES as u64) as usize;
        blocks.push(tone_block(&source_id, RATE, frames, 2, offset));
        offset += frames as u64;
    }

    // Three live polls, then the accepted drain path owns the rest of the source.
    let mut session = NormalizingAudioSession::new(ScriptedSession::new(blocks), source_id.clone());
    let out = read_stream(&mut session, 3).expect("the adapter must accept the source");
    let samples = interleave(&out);
    let frames = expected_frames(SOURCE_FRAMES, RATE);
    assert_eq!(
        samples.len() as u64 / 2,
        frames,
        "the normalized stream must cover exactly the accepted source duration"
    );

    // Every block is on the accepted 48 kHz grid, within the accepted 100 ms ceiling.
    let mut offset = 0u64;
    for block in &out {
        assert_eq!(block.source_id, source_id);
        assert_eq!(block.sample_rate.get(), NORMALIZED_RATE);
        assert_eq!(block.channels.get(), 2);
        assert!(block.samples.len() / 2 <= 4_800);
        assert_eq!(block.timestamp, grid_timestamp(offset, NORMALIZED_RATE));
        offset += (block.samples.len() / 2) as u64;
    }
    assert_eq!(offset, frames);

    assert_tone_content(&samples, frames, 44_100, 2e-3);

    // The tail is real audio, not emitted zero padding in place of it.
    let tail = &samples[samples.len() - 2 * OUTPUT_BLOCK_FRAMES..];
    assert!(
        rms(tail) >= 0.1,
        "the last 10 ms of the normalized stream are silence, so the tail was replaced by padding"
    );

    let counters = session.counters();
    println!(
        "frame-0206 evidence: 44.1 kHz in_blocks={} frames_in={} frames_out={} delay_trimmed={} padding_dropped={} snapped={} blocks_out={}",
        counters.blocks_in,
        counters.frames_in,
        counters.frames_out,
        counters.delay_frames_trimmed,
        counters.padding_frames_dropped,
        counters.snapped_frames,
        counters.blocks_out
    );
    assert_eq!(
        counters.blocks_in,
        (SOURCE_FRAMES / BLOCK_FRAMES as u64) + 1
    );
    assert_eq!(counters.frames_in, SOURCE_FRAMES);
    assert_eq!(counters.frames_out, frames);
    assert_eq!(
        counters.snapped_frames, 0,
        "an exact source grid is not jitter"
    );
    assert!(
        counters.delay_frames_trimmed > 0,
        "the resampler's real latency was never trimmed"
    );
    assert!(
        counters.padding_frames_dropped > 0,
        "the drain flush must produce padding that is excluded from the duration"
    );

    // The accepted mixer takes every normalized block without a gap or a rejection and
    // returns exactly the same frames.
    let routes = [AudioRouteSelection {
        controls: Default::default(),
        muted: false,
        source_id: source_id.clone(),
        route: AudioRoute::MainMix,
    }];
    let mut mixer = AlignedPcmProcessor::new(&routes, grid_timestamp(0, NORMALIZED_RATE))
        .expect("the mixer plan is valid");
    for block in out {
        if let Err((error, _)) = mixer.push(block) {
            panic!("the mixer rejected a normalized block: {}", error.message);
        }
    }
    mixer.finish().expect("the tail boundary is representable");
    let mut mixed = Vec::new();
    while let Some(quantum) = mixer
        .poll(MediaTimestamp(T0 + 1_000_000_000))
        .expect("the mixer must emit the frozen tail")
    {
        assert!(quantum.gaps.is_empty(), "the normalized stream has a gap");
        assert_eq!(quantum.clipped_samples, 0);
        for track in &quantum.tracks {
            mixed.extend_from_slice(&track.samples);
        }
    }
    assert_eq!(
        mixed, samples,
        "the mixer must reproduce exactly the normalized frames"
    );

    // A real source discontinuity is an explicit error, never silence and never a
    // resampler-latency artefact.
    let jump = vec![
        tone_block(&source_id, RATE, 441, 2, 0),
        tone_block(&source_id, RATE, 441, 2, 1_323),
    ];
    let mut lost = NormalizingAudioSession::new(ScriptedSession::new(jump), source_id.clone());
    assert!(
        lost.try_pcm()
            .expect("the first block is accepted")
            .is_none(),
        "the first block cannot produce a whole resampler chunk"
    );
    let Err(error) = lost.try_pcm() else {
        panic!("a 20 ms source jump must fail instead of becoming a gap");
    };
    assert_eq!(error.code, ErrorCode::SourceLost);

    // 96 kHz: the same connection at half the frames, and the same content.
    const FAST: u32 = 96_000;
    const FAST_FRAMES: u64 = 4_800;
    let mut fast_blocks = Vec::new();
    let mut offset = 0u64;
    while offset < FAST_FRAMES {
        let frames = (FAST_FRAMES - offset).min(960) as usize;
        fast_blocks.push(tone_block(&source_id, FAST, frames, 2, offset));
        offset += frames as u64;
    }
    let mut fast =
        NormalizingAudioSession::new(ScriptedSession::new(fast_blocks), source_id.clone());
    let fast_out = read_stream(&mut fast, 5).expect("the adapter must accept the 96 kHz source");
    let fast_samples = interleave(&fast_out);
    let fast_frames = expected_frames(FAST_FRAMES, FAST);
    assert_eq!(fast_frames, 2_400);
    assert_eq!(fast_samples.len() as u64 / 2, fast_frames);
    assert_tone_content(&fast_samples, fast_frames, 96_000, 1e-2);
    assert_eq!(fast.counters().frames_in, FAST_FRAMES);
    assert_eq!(fast.counters().frames_out, fast_frames);
}

#[test]
fn forty_eight_kilohertz_bypass_is_unchanged_and_unsupported_formats_are_refused() {
    const RATE: u32 = 48_000;
    let source_id = AudioSourceId("wasapi-mic:frame-0206-bypass".into());

    // A 48 kHz stereo source keeps its own samples, block boundaries and timestamps.
    let sizes = [300usize, 700, 480];
    let mut blocks = Vec::new();
    let mut offset = 0u64;
    for frames in sizes {
        blocks.push(tone_block(&source_id, RATE, frames, 2, offset));
        offset += frames as u64;
    }
    let expected: Vec<(MediaTimestamp, Vec<f32>)> = blocks
        .iter()
        .map(|block| (block.timestamp, block.samples.clone()))
        .collect();
    let mut session = NormalizingAudioSession::new(ScriptedSession::new(blocks), source_id.clone());
    let out = read_stream(&mut session, 100).expect("the adapter must accept the bypass source");
    assert_eq!(out.len(), expected.len());
    for (block, (timestamp, samples)) in out.iter().zip(&expected) {
        assert_eq!(block.timestamp, *timestamp);
        assert_eq!(block.samples, *samples);
        assert_eq!(block.sample_rate.get(), NORMALIZED_RATE);
        assert_eq!(block.channels.get(), 2);
    }
    assert!(
        !session.is_resampling(),
        "48 kHz input must not be resampled"
    );
    let counters = session.counters();
    println!(
        "frame-0206 evidence: 48 kHz bypass in_blocks={} frames_in={} frames_out={} delay_trimmed={} padding_dropped={} snapped={} blocks_out={}",
        counters.blocks_in,
        counters.frames_in,
        counters.frames_out,
        counters.delay_frames_trimmed,
        counters.padding_frames_dropped,
        counters.snapped_frames,
        counters.blocks_out
    );
    assert_eq!(counters.frames_in, 1_480);
    assert_eq!(counters.frames_out, 1_480);
    assert_eq!(counters.delay_frames_trimmed, 0);
    assert_eq!(counters.padding_frames_dropped, 0);
    assert_eq!(counters.snapped_frames, 0);

    // A 48 kHz mono source reaches the same stereo line by duplication, unchanged in
    // time and frame count.
    let mono = tone_block(&source_id, RATE, 240, 1, 0);
    let mono_timestamp = mono.timestamp;
    let raw = mono.samples.clone();
    let mut session =
        NormalizingAudioSession::new(ScriptedSession::new(vec![mono]), source_id.clone());
    let out = read_stream(&mut session, 100).expect("the adapter must accept the mono source");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].timestamp, mono_timestamp);
    assert_eq!(out[0].channels.get(), 2);
    assert_eq!(out[0].samples.len(), raw.len() * 2);
    for (frame, value) in raw.iter().enumerate() {
        assert_eq!(out[0].samples[frame * 2], *value);
        assert_eq!(out[0].samples[frame * 2 + 1], *value);
    }

    // Unsupported, malformed and oversized native blocks are explicit refusals.
    assert_eq!(
        code_of(tone_block(&source_id, 32_000, 320, 2, 0), &source_id),
        ErrorCode::NotImplemented
    );
    assert_eq!(
        code_of(block(&source_id, RATE, 6, 0, vec![0.1; 48 * 6]), &source_id),
        ErrorCode::NotImplemented
    );
    assert_eq!(
        code_of(
            block(&source_id, RATE, 2, 0, vec![0.1; 4_801 * 2]),
            &source_id
        ),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        code_of(block(&source_id, RATE, 2, 0, vec![f32::NAN; 2]), &source_id),
        ErrorCode::InvalidRequest
    );

    // A source that changes its native format mid-session is a lost source, not a
    // silently re-normalized one. The first block is already-delivered bypass audio.
    let mut changed = NormalizingAudioSession::new(
        ScriptedSession::new(vec![
            tone_block(&source_id, RATE, 240, 1, 0),
            tone_block(&source_id, 44_100, 240, 1, 240),
        ]),
        source_id.clone(),
    );
    let mut failure = None;
    for _ in 0..4 {
        match changed.try_pcm() {
            Ok(_) => continue,
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let error = failure.expect("a mid-session format change must fail");
    assert_eq!(error.code, ErrorCode::SourceLost);
}
