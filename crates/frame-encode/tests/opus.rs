// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Focused Opus checks: a real encode/decode round trip through pre-skip and the
//! final discard padding, plus partial-frame flush and closed admission.

use frame_audio::TrackPcm;
use frame_clock::{MediaTimestamp, sample_timestamp};
use frame_encode::opus::{CHANNELS, OpusTrackEncoder, SAMPLE_RATE};
use std::f32::consts::PI;
use std::num::{NonZeroU16, NonZeroU32};

/// One 10 ms mixer quantum: half an Opus frame.
const BLOCK: usize = 480;
/// One 20 ms Opus frame.
const PACKET: u64 = 960;

fn offset(frames: u64) -> u64 {
    sample_timestamp(frames, NonZeroU32::new(SAMPLE_RATE).unwrap())
        .unwrap()
        .0
}

fn pcm(track_index: usize, at: MediaTimestamp, first_frame: u64, frames: usize) -> TrackPcm {
    let mut samples = Vec::with_capacity(frames * usize::from(CHANNELS));
    for index in 0..frames as u64 {
        let phase = 2.0 * PI * 1_000.0 * ((first_frame + index) as f32) / SAMPLE_RATE as f32;
        let value = 0.5 * phase.sin();
        samples.push(value);
        samples.push(value);
    }
    block(track_index, at, samples)
}

fn block(track_index: usize, at: MediaTimestamp, samples: Vec<f32>) -> TrackPcm {
    TrackPcm {
        track_index,
        timestamp: at,
        sample_rate: NonZeroU32::new(SAMPLE_RATE).unwrap(),
        channels: NonZeroU16::new(CHANNELS).unwrap(),
        samples,
    }
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

#[test]
fn real_opus_round_trip_keeps_the_input_length_after_pre_skip_and_trim() {
    let origin = MediaTimestamp(1_000_000_000);
    let mut encoder = OpusTrackEncoder::new(0, origin).unwrap();
    let pre_skip = encoder.pre_skip();
    assert!(pre_skip > 0, "libopus reports a real lookahead");

    let mut packets = Vec::new();
    let mut input = Vec::new();
    for count in 0..3u64 {
        let track = pcm(
            0,
            MediaTimestamp(origin.0 + offset(count * BLOCK as u64)),
            count * BLOCK as u64,
            BLOCK,
        );
        input.extend_from_slice(&track.samples);
        if let Some(packet) = encoder.submit(&track).unwrap() {
            packets.push(packet);
        }
    }
    assert_eq!(packets.len(), 1, "three 10 ms blocks fill one 20 ms frame");
    let finish = encoder.finish().unwrap();
    packets.extend(finish.packets);
    assert_eq!(packets.len(), 2, "the flush completes the pending frame");
    assert_eq!(finish.input_frames, 3 * BLOCK as u64);

    // Packet timestamps stay on the encoded-sample axis before trimming.
    for (index, packet) in packets.iter().enumerate() {
        assert_eq!(
            packet.pts,
            MediaTimestamp(origin.0 + offset(index as u64 * PACKET))
        );
        assert_eq!(packet.dts, packet.pts);
        assert_eq!(packet.duration_ns, 20_000_000);
        assert_eq!(packet.track_id, 0);
        assert!(!packet.data.is_empty());
    }

    // Real decode: every packet carries exactly one 20 ms frame.
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut buffer = vec![0.0f32; 5_760 * usize::from(CHANNELS)];
    let mut decoded = Vec::new();
    for packet in &packets {
        let frames = decoder
            .decode_float(&packet.data, &mut buffer, false)
            .unwrap();
        assert_eq!(frames, PACKET as usize);
        decoded.extend_from_slice(&buffer[..frames * usize::from(CHANNELS)]);
    }

    // Pre-skip and discard padding remove exactly the samples the codec added.
    let decoded_frames = decoded.len() / usize::from(CHANNELS);
    assert_eq!(
        decoded_frames as u64,
        u64::from(finish.pre_skip) + u64::from(finish.discard_padding) + finish.input_frames
    );
    assert_eq!(finish.pre_skip, pre_skip);
    let start = usize::from(finish.pre_skip) * usize::from(CHANNELS);
    let end = decoded.len() - usize::from(finish.discard_padding) * usize::from(CHANNELS);
    let trimmed = &decoded[start..end];
    assert_eq!(trimmed.len(), input.len());
    assert_eq!(trimmed.len() / usize::from(CHANNELS), 3 * BLOCK);

    // The trimmed signal is the encoded input, not silence or padding.
    let ratio = rms(trimmed) / rms(&input);
    assert!(
        (0.7..1.3).contains(&ratio),
        "decoded energy ratio {ratio} is not the input tone"
    );
}

#[test]
fn partial_final_frame_flushes_once_and_admission_never_reopens() {
    let origin = MediaTimestamp(4_000);
    let mut encoder = OpusTrackEncoder::new(1, origin).unwrap();
    let track = pcm(1, origin, 0, BLOCK);
    assert!(
        encoder.submit(&track).unwrap().is_none(),
        "half an Opus frame produces no packet"
    );

    let finish = encoder.finish().unwrap();
    assert_eq!(finish.packets.len(), 1);
    assert_eq!(finish.packets[0].pts, origin);
    assert_eq!(finish.input_frames, BLOCK as u64);
    assert_eq!(
        u64::from(finish.pre_skip) + u64::from(finish.discard_padding) + finish.input_frames,
        PACKET,
        "pre-skip plus discard padding accounts for the padded decode length"
    );
    assert!(finish.discard_padding < PACKET as u16);

    assert!(
        encoder.finish().is_err(),
        "a repeated finish is rejected, not repeated"
    );
    assert!(
        encoder.submit(&track).is_err(),
        "input after finish is rejected"
    );

    // A 900-frame tail plus lookahead needs two flush packets, not one.
    let mut encoder = OpusTrackEncoder::new(1, origin).unwrap();
    encoder.submit(&track).unwrap();
    let tail = pcm(1, MediaTimestamp(origin.0 + offset(480)), 480, 420);
    assert!(encoder.submit(&tail).unwrap().is_none());
    let finish = encoder.finish().unwrap();
    assert_eq!(finish.packets.len(), 2);
    assert_eq!(finish.input_frames, 900);
    assert_eq!(
        u64::from(finish.pre_skip) + u64::from(finish.discard_padding) + finish.input_frames,
        2 * PACKET
    );
}

#[test]
fn empty_tracks_emit_nothing_and_invalid_blocks_change_no_state() {
    let mut empty = OpusTrackEncoder::new(0, MediaTimestamp(7)).unwrap();
    let finish = empty.finish().unwrap();
    assert!(finish.packets.is_empty(), "no input means no packet");
    assert_eq!(finish.input_frames, 0);
    assert_eq!(finish.discard_padding, 0);
    assert_eq!(finish.pre_skip, empty.pre_skip());

    let origin = MediaTimestamp(0);
    let mut encoder = OpusTrackEncoder::new(0, origin).unwrap();
    assert_eq!(encoder.track_id(), 0);
    let wrong_track = pcm(1, origin, 0, BLOCK);
    assert!(encoder.submit(&wrong_track).is_err());

    let mut wrong_rate = pcm(0, origin, 0, BLOCK);
    wrong_rate.sample_rate = NonZeroU32::new(44_100).unwrap();
    assert!(encoder.submit(&wrong_rate).is_err());

    let mut wrong_channels = pcm(0, origin, 0, BLOCK);
    wrong_channels.channels = NonZeroU16::new(1).unwrap();
    assert!(encoder.submit(&wrong_channels).is_err());

    assert!(encoder.submit(&block(0, origin, Vec::new())).is_err());
    assert!(
        encoder.submit(&pcm(0, origin, 0, BLOCK * 2)).is_err(),
        "more than 480 frames per block is rejected"
    );
    assert!(
        encoder
            .submit(&block(0, origin, vec![0.0, 0.0, 0.0]))
            .is_err(),
        "a half stereo frame is rejected"
    );
    assert!(
        encoder
            .submit(&block(0, origin, vec![f32::NAN, 0.0]))
            .is_err(),
        "a non-finite sample is rejected"
    );
    assert!(
        encoder
            .submit(&pcm(0, MediaTimestamp(1), 0, BLOCK))
            .is_err(),
        "a broken sample timeline is rejected"
    );

    // Every rejection above left the encoder untouched.
    assert!(encoder.submit(&pcm(0, origin, 0, BLOCK)).unwrap().is_none());
    assert_eq!(encoder.finish().unwrap().input_frames, BLOCK as u64);

    // The input fits, but the second padded codec packet would end outside the
    // media timeline. Reject it before admission; the earlier block still flushes.
    let origin = MediaTimestamp(u64::MAX - 39_999_999);
    let mut encoder = OpusTrackEncoder::new(0, origin).unwrap();
    assert!(encoder.submit(&pcm(0, origin, 0, BLOCK)).unwrap().is_none());
    let tail = pcm(0, MediaTimestamp(origin.0 + offset(480)), 480, 420);
    assert!(encoder.submit(&tail).is_err());
    let finish = encoder.finish().unwrap();
    assert_eq!(finish.input_frames, 480);
    assert_eq!(finish.packets.len(), 1);
}
