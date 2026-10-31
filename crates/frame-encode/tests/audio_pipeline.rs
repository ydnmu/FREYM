// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Focused encoded-pipeline checks against a scripted in-process session double:
//! main/clean separation and the exact short tail through a real Opus decode, and
//! cleanup on a source failure, an encoder rejection and an early stop.

use frame_audio::{
    AudioBackend, AudioCaptureBackend, AudioContext, AudioPipeline, AudioSession,
    DrainableAudioSession, PcmBlock, PcmDrainPoll, TrackKind,
};
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline, sample_timestamp};
use frame_contracts::{
    AudioRoute, AudioRouteSelection, AudioSource, AudioSourceId, ErrorCode, RecorderError,
    RecoveryStatus,
};
use frame_encode::EncodedPacket;
use frame_encode::audio_pipeline::{
    EncodedAudioDrainPoll, EncodedAudioPipeline, MAX_FINISH_PACKETS, MAX_LIVE_PACKETS, MAX_TRACKS,
};
use frame_encode::opus::{CHANNELS, SAMPLE_RATE};
use std::{
    cell::RefCell,
    collections::VecDeque,
    f32::consts::PI,
    num::{NonZeroU16, NonZeroU32},
    rc::Rc,
    sync::{Arc, Mutex},
};

/// A nonzero recording origin: the pipeline and the encoders must share exactly it.
const ORIGIN: u64 = 1_000_000_000;
/// One 10 ms mixer quantum: half an Opus frame.
const BLOCK: usize = 480;
/// The accepted input of the main scenario: two live quanta plus a 200-frame tail.
const TAIL_FRAMES: u64 = 200;
const INPUT_FRAMES: u64 = 2 * BLOCK as u64 + TAIL_FRAMES;
/// One 20 ms Opus packet.
const PACKET_FRAMES: u64 = 960;

#[derive(Default)]
struct Observed {
    /// Source IDs passed to `start_audio`, in call order.
    opened: Vec<String>,
    /// Source IDs whose session `stop` returned, in call order.
    stopped: Vec<String>,
    /// Source IDs whose session was dropped, in call order.
    dropped: Vec<String>,
}

/// One scripted `try_pcm` result.
type Step = Result<Option<PcmBlock>, RecorderError>;
/// One scripted `poll_drain` result.
type DrainStep = Result<PcmDrainPoll, RecorderError>;

struct FakeSession {
    id: String,
    observed: Rc<RefCell<Observed>>,
    script: VecDeque<Step>,
    drain_script: VecDeque<DrainStep>,
}

impl AudioSession for FakeSession {
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        self.script.pop_front().unwrap_or(Ok(None))
    }

    fn stop(&mut self) -> Result<(), RecorderError> {
        self.observed.borrow_mut().stopped.push(self.id.clone());
        Ok(())
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        self.observed.borrow_mut().dropped.push(self.id.clone());
    }
}

impl DrainableAudioSession for FakeSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }

    /// An exhausted drain script is a completed source, like a real finished worker.
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        self.drain_script
            .pop_front()
            .unwrap_or(Ok(PcmDrainPoll::Drained))
    }
}

#[derive(Default)]
struct FakeBackend {
    observed: Rc<RefCell<Observed>>,
    scripts: Vec<(String, Vec<Step>)>,
    drain_scripts: Vec<(String, Vec<DrainStep>)>,
}

impl FakeBackend {
    fn script(&mut self, id: &str, steps: Vec<Step>) {
        self.scripts.push((id.to_string(), steps));
    }

    fn drain_script(&mut self, id: &str, steps: Vec<DrainStep>) {
        self.drain_scripts.push((id.to_string(), steps));
    }
}

impl AudioBackend for FakeBackend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        Ok(Vec::new())
    }
}

impl AudioCaptureBackend for FakeBackend {
    type Session = FakeSession;

    fn start_audio(
        &mut self,
        source: &AudioSourceId,
        _context: AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        self.observed.borrow_mut().opened.push(source.0.clone());
        let script = self
            .scripts
            .iter()
            .position(|(id, _)| id == &source.0)
            .map(|index| VecDeque::from(self.scripts.remove(index).1))
            .unwrap_or_default();
        let drain_script = self
            .drain_scripts
            .iter()
            .position(|(id, _)| id == &source.0)
            .map(|index| VecDeque::from(self.drain_scripts.remove(index).1))
            .unwrap_or_default();
        Ok(FakeSession {
            id: source.0.clone(),
            observed: Rc::clone(&self.observed),
            script,
            drain_script,
        })
    }
}

fn route(id: &str, route: AudioRoute) -> AudioRouteSelection {
    AudioRouteSelection {
        controls: Default::default(),
        muted: false,
        source_id: AudioSourceId(id.into()),
        route,
    }
}

fn rate() -> NonZeroU32 {
    NonZeroU32::new(SAMPLE_RATE).unwrap()
}

/// The mixer's own time derivation: the shared origin plus an exact sample count.
fn timestamp_of(first_frame: u64) -> MediaTimestamp {
    MediaTimestamp(ORIGIN + sample_timestamp(first_frame, rate()).unwrap().0)
}

/// One tone run, so a block and the expectation built from it are the same signal.
fn tone(frames: u64, frequency: f32, amplitude: f32, first_frame: u64) -> Vec<f32> {
    (0..frames)
        .map(|index| {
            let phase = 2.0 * PI * frequency * ((first_frame + index) as f32) / SAMPLE_RATE as f32;
            amplitude * phase.sin()
        })
        .collect()
}

fn block(id: &str, first_frame: u64, channels: u16, samples: Vec<f32>) -> PcmBlock {
    PcmBlock {
        source_id: AudioSourceId(id.into()),
        timestamp: timestamp_of(first_frame),
        sample_rate: rate(),
        channels: NonZeroU16::new(channels).unwrap(),
        samples,
    }
}

fn mono_block(id: &str, first_frame: u64, frames: u64, frequency: f32, amplitude: f32) -> PcmBlock {
    block(
        id,
        first_frame,
        1,
        tone(frames, frequency, amplitude, first_frame),
    )
}

fn stereo_block(
    id: &str,
    first_frame: u64,
    frames: u64,
    frequency: f32,
    left: f32,
    right: f32,
) -> PcmBlock {
    let mut samples = Vec::with_capacity(frames as usize * 2);
    for index in 0..frames {
        let phase = 2.0 * PI * frequency * ((first_frame + index) as f32) / SAMPLE_RATE as f32;
        samples.push(left * phase.sin());
        samples.push(right * phase.sin());
    }
    block(id, first_frame, 2, samples)
}

fn channel(samples: &[f32], index: usize) -> Vec<f32> {
    samples.iter().skip(index).step_by(2).copied().collect()
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

/// The established lossy-codec tolerance: the decoded tone, not silence or a mix.
fn assert_tone(decoded: &[f32], expected: &[f32], what: &str) {
    let ratio = rms(decoded) / rms(expected);
    assert!(
        (0.7..1.3).contains(&ratio),
        "{what}: decoded energy ratio {ratio} is not the source tone"
    );
}

/// Real decode of one track's packets, trimmed by the reported pre-skip and padding.
fn decode_track(packets: &[&EncodedPacket], pre_skip: u16, discard_padding: u16) -> Vec<f32> {
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut buffer = vec![0.0f32; 5_760 * usize::from(CHANNELS)];
    let mut decoded = Vec::new();
    for packet in packets {
        let frames = decoder
            .decode_float(&packet.data, &mut buffer, false)
            .unwrap();
        assert_eq!(frames, PACKET_FRAMES as usize);
        decoded.extend_from_slice(&buffer[..frames * usize::from(CHANNELS)]);
    }
    let start = usize::from(pre_skip) * usize::from(CHANNELS);
    let end = decoded.len() - usize::from(discard_padding) * usize::from(CHANNELS);
    decoded[start..end].to_vec()
}

fn source_lost(message: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message: message.to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Re-select the audio source and start a new session.".into(),
    }
}

#[test]
fn rotation_decodes_each_opus_instance_with_its_own_trim_without_reopening_sources() {
    let mut backend = FakeBackend::default();
    backend.script(
        "a",
        (0..4)
            .map(|index| {
                Ok(Some(stereo_block(
                    "a",
                    index * 480,
                    480,
                    1_000.0,
                    0.3,
                    -0.3,
                )))
            })
            .collect(),
    );
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();
    let mut previous = Vec::new();
    for _ in 0..2 {
        previous.extend(encoded.poll(timestamp_of(1920)).unwrap());
    }
    let finish = encoded.rotate_segment().unwrap();
    previous.extend(finish.packets);
    let track = &finish.tracks[0];
    assert_eq!(track.input_frames, 960);
    assert_eq!(encoded.segment_origin(), timestamp_of(960));
    let previous_refs: Vec<_> = previous.iter().collect();
    assert_eq!(
        decode_track(&previous_refs, track.pre_skip, track.discard_padding).len(),
        1920
    );
    assert!(backend.observed.borrow().stopped.is_empty());
    let mut next = Vec::new();
    for _ in 0..2 {
        next.extend(encoded.poll(timestamp_of(1920)).unwrap());
    }
    let finish = encoded.rotate_segment().unwrap();
    next.extend(finish.packets);
    let track = &finish.tracks[0];
    assert_eq!(next[0].pts, timestamp_of(960));
    assert_eq!(track.input_frames, 960);
    let next_refs: Vec<_> = next.iter().collect();
    assert_eq!(
        decode_track(&next_refs, track.pre_skip, track.discard_padding).len(),
        1920
    );
    assert_eq!(backend.observed.borrow().opened, vec!["a"]);
    assert!(backend.observed.borrow().stopped.is_empty());
}

#[test]
fn configured_mono_downmix_averages_stereo_before_the_real_encoder() {
    let mut backend = FakeBackend::default();
    backend.script(
        "a",
        (0..3)
            .map(|i| Ok(Some(stereo_block("a", i * 480, 480, 1000.0, 0.3, -0.3))))
            .collect(),
    );
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::SeparateOnly)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let settings = frame_contracts::AudioSettings {
        channels: frame_contracts::AudioChannels::Mono,
        bitrate_bps: 192_000,
        ..frame_contracts::AudioSettings::default()
    };
    let mut encoded =
        EncodedAudioPipeline::with_settings(pipeline, MediaTimestamp(ORIGIN), settings).unwrap();
    let mut packets = Vec::new();
    for _ in 0..3 {
        packets.extend(encoded.poll(timestamp_of(1440)).unwrap());
    }
    packets.extend(encoded.rotate_segment().unwrap().packets);
    let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
    for packet in packets {
        let mut samples = [0.0; 960];
        assert_eq!(
            decoder
                .decode_float(&packet.data, &mut samples, false)
                .unwrap(),
            960
        );
        assert!(samples.iter().all(|s| s.abs() < 0.0001));
    }
}

#[test]
fn paused_pcm_is_excluded_from_contiguous_opus_samples() {
    let mut backend = FakeBackend::default();
    backend.script(
        "mic",
        vec![
            Ok(Some(mono_block("mic", 0, 480, 1_000.0, 0.2))),
            Ok(Some(mono_block("mic", 480, 480, 1_000.0, 0.2))),
            Ok(Some(mono_block("mic", 960, 480, 1_000.0, 0.9))),
            Ok(Some(mono_block("mic", 5_760, 480, 1_000.0, 0.2))),
            Ok(Some(mono_block("mic", 6_240, 480, 1_000.0, 0.2))),
        ],
    );
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[route("mic", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();
    let timeline = Arc::new(Mutex::new(PauseTimeline::default()));
    encoded.set_pause_timeline(Arc::clone(&timeline));
    let mut packets = encoded.poll(timestamp_of(480)).unwrap();
    timeline.lock().unwrap().pause(timestamp_of(960)).unwrap();
    // The second pre-pause block is still queued when Pause arrives. The owner
    // services it before Resume, then discards the block inside the pause.
    assert_eq!(encoded.poll_sources().unwrap(), 1);
    packets.extend(encoded.poll(timestamp_of(960)).unwrap());
    timeline
        .lock()
        .unwrap()
        .resume(timestamp_of(5_760))
        .unwrap();
    packets.extend(encoded.poll(timestamp_of(1_440)).unwrap());
    packets.extend(encoded.poll(timestamp_of(1_920)).unwrap());
    encoded.request_drain().unwrap();
    let finish = loop {
        match encoded.poll_drain().unwrap() {
            EncodedAudioDrainPoll::Packets(batch) => packets.extend(batch),
            EncodedAudioDrainPoll::Finished(finish) => break finish,
            EncodedAudioDrainPoll::Pending => continue,
            EncodedAudioDrainPoll::Drained => panic!("drain completed without a finish report"),
        }
    };
    let track = &finish.tracks[0];
    packets.extend(finish.packets);
    assert_eq!(track.input_frames, 1_920);
    assert_eq!(packets[0].pts, timestamp_of(0));
    assert!(
        packets
            .windows(2)
            .all(|pair| pair[1].pts.0 - pair[0].pts.0 <= 20_000_000)
    );
    let decoded = decode_track(
        &packets.iter().collect::<Vec<_>>(),
        track.pre_skip,
        track.discard_padding,
    );
    assert_eq!(decoded.len(), 1_920 * usize::from(CHANNELS));
}

#[test]
fn live_and_drain_keep_main_and_clean_separate_and_encode_the_short_tail_exactly() {
    let mut backend = FakeBackend::default();
    // Two 10 ms live blocks per source, then one 200-frame tail during the drain.
    backend.script(
        "mic",
        vec![
            Ok(Some(mono_block("mic", 0, BLOCK as u64, 1_000.0, 0.25))),
            Ok(Some(mono_block("mic", 480, BLOCK as u64, 1_000.0, 0.25))),
        ],
    );
    backend.script(
        "app",
        vec![
            Ok(Some(stereo_block(
                "app",
                0,
                BLOCK as u64,
                400.0,
                0.5,
                -0.25,
            ))),
            Ok(Some(stereo_block(
                "app",
                480,
                BLOCK as u64,
                400.0,
                0.5,
                -0.25,
            ))),
        ],
    );
    backend.drain_script(
        "mic",
        vec![
            Ok(PcmDrainPoll::Block(mono_block(
                "mic",
                960,
                TAIL_FRAMES,
                1_000.0,
                0.25,
            ))),
            Ok(PcmDrainPoll::Drained),
        ],
    );
    backend.drain_script(
        "app",
        vec![
            Ok(PcmDrainPoll::Block(stereo_block(
                "app",
                960,
                TAIL_FRAMES,
                400.0,
                0.5,
                -0.25,
            ))),
            Ok(PcmDrainPoll::Drained),
        ],
    );
    let observed = Rc::clone(&backend.observed);
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("mic", AudioRoute::MainMixAndSeparate),
            route("app", AudioRoute::SeparateOnly),
            route("ignored", AudioRoute::Ignore),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    // The same origin reaches the encoders, and no native session is reopened.
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();

    // The frozen plan and the pre-skip are readable before any PCM arrives.
    let track_count = {
        let plan = encoded.tracks();
        assert_eq!(plan.len(), 3);
        assert!(plan.len() <= MAX_TRACKS);
        assert_eq!(plan[0].kind, TrackKind::MainMix);
        assert_eq!(plan[0].sources, vec![AudioSourceId("mic".into())]);
        assert_eq!(plan[1].kind, TrackKind::Clean(AudioSourceId("app".into())));
        assert_eq!(plan[2].kind, TrackKind::Clean(AudioSourceId("mic".into())));
        plan.len()
    };
    assert_eq!(encoded.origin(), MediaTimestamp(ORIGIN));
    let pre_skips: Vec<u16> = (0..track_count)
        .map(|index| encoded.pre_skip(index).unwrap())
        .collect();
    assert!(
        pre_skips
            .iter()
            .all(|&pre_skip| pre_skip > 0 && pre_skip == pre_skips[0])
    );
    assert_eq!(encoded.pre_skip(track_count), None);
    // The ignored route is planned but never opened and never gets an encoder.
    assert_eq!(observed.borrow().opened, vec!["app", "mic"]);

    // Live: the first quantum only fills the encoders' pending Opus frame.
    assert!(encoded.poll(MediaTimestamp(0)).unwrap().is_empty());
    let live = encoded.poll(MediaTimestamp(0)).unwrap();
    assert!(live.len() <= MAX_LIVE_PACKETS);
    assert_eq!(
        live.iter()
            .map(|packet| packet.track_id)
            .collect::<Vec<_>>(),
        vec![0, 1, 2],
        "one packet per track, in PTS/track-id order"
    );
    assert!(live.iter().all(|packet| packet.pts == timestamp_of(0)));
    // One quantum per poll: the exhausted live scripts yield nothing further.
    assert!(encoded.poll(MediaTimestamp(0)).unwrap().is_empty());

    encoded.request_drain().unwrap();
    // The live path is invalid while the drain owns the tail and disturbs nothing.
    assert_eq!(
        encoded.poll(MediaTimestamp(0)).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    // The 200-frame tail is not covered yet: `Pending` is never completion.
    assert!(matches!(
        encoded.poll_drain().unwrap(),
        EncodedAudioDrainPoll::Pending
    ));
    // The short final quantum is encoded like any other; its 200 frames only fill the
    // encoders' pending frame, so no packet is ready yet.
    match encoded.poll_drain().unwrap() {
        EncodedAudioDrainPoll::Packets(packets) => assert!(packets.is_empty()),
        other => panic!("expected the encoded short tail, got {other:?}"),
    }
    // Only after the pipeline drained is every encoder finished, exactly once.
    let finish = match encoded.poll_drain().unwrap() {
        EncodedAudioDrainPoll::Finished(finish) => finish,
        other => panic!("expected the one-shot finish report, got {other:?}"),
    };
    assert!(finish.packets.len() <= MAX_FINISH_PACKETS);
    assert_eq!(finish.packets.len(), 3);
    let order: Vec<(u64, u32)> = finish
        .packets
        .iter()
        .map(|packet| (packet.pts.0, packet.track_id))
        .collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted, "a batch is ordered by PTS then track id");
    assert!(
        finish
            .packets
            .iter()
            .all(|packet| packet.pts == timestamp_of(960))
    );
    assert_eq!(finish.tracks.len(), 3);
    assert!(finish.tracks.iter().all(|track| track.packets.is_empty()));
    assert!(
        finish
            .tracks
            .iter()
            .all(|track| track.input_frames == INPUT_FRAMES)
    );
    assert_eq!(
        finish
            .tracks
            .iter()
            .map(|track| track.pre_skip)
            .collect::<Vec<_>>(),
        pre_skips,
        "the finish metadata stays in plan order"
    );
    // Mixer drained and every session closed: the verdict is permanent and the flush
    // is never repeated, even by a later stop.
    assert_eq!(observed.borrow().stopped, vec!["app", "mic"]);
    assert_eq!(observed.borrow().dropped, vec!["app", "mic"]);
    assert!(matches!(
        encoded.poll_drain().unwrap(),
        EncodedAudioDrainPoll::Drained
    ));
    assert!(matches!(
        encoded.poll_drain().unwrap(),
        EncodedAudioDrainPoll::Drained
    ));
    encoded.stop().unwrap();
    assert!(matches!(
        encoded.poll_drain().unwrap(),
        EncodedAudioDrainPoll::Drained
    ));
    assert!(encoded.failure().is_none());

    // The live packet and the flush packet of each track, decoded for real.
    let mut per_track: Vec<Vec<&EncodedPacket>> = vec![Vec::new(); track_count];
    for packet in live.iter().chain(finish.packets.iter()) {
        per_track[usize::try_from(packet.track_id).unwrap()].push(packet);
    }
    assert!(per_track.iter().all(|packets| packets.len() == 2));
    let decoded: Vec<Vec<f32>> = per_track
        .iter()
        .enumerate()
        .map(|(index, packets)| {
            decode_track(
                packets,
                finish.tracks[index].pre_skip,
                finish.tracks[index].discard_padding,
            )
        })
        .collect();
    // Every track decodes to exactly the accepted 960 + 200 frames: the pre-skip and
    // the discard padding account for the whole padded decode, short tail included.
    for samples in &decoded {
        assert_eq!(samples.len(), INPUT_FRAMES as usize * usize::from(CHANNELS));
    }

    let mic = tone(INPUT_FRAMES, 1_000.0, 0.25, 0);
    let app_left = tone(INPUT_FRAMES, 400.0, 0.5, 0);
    let app_right = tone(INPUT_FRAMES, 400.0, -0.25, 0);
    // The main mix carries the mic alone: app is separate-only, never mixed in.
    assert_tone(&channel(&decoded[0], 0), &mic, "main left");
    assert_tone(&channel(&decoded[0], 1), &mic, "main right");
    let mixed: Vec<f32> = mic
        .iter()
        .zip(&app_left)
        .map(|(mic, app)| mic + app)
        .collect();
    let mixed_ratio = rms(&channel(&decoded[0], 0)) / rms(&mixed);
    assert!(
        !(0.7..1.3).contains(&mixed_ratio),
        "the main mix must not contain the separate-only source (ratio {mixed_ratio})"
    );
    // The clean stems carry their own source, with their own channel content.
    assert_tone(&channel(&decoded[1], 0), &app_left, "clean app left");
    assert_tone(&channel(&decoded[1], 1), &app_right, "clean app right");
    assert_tone(&channel(&decoded[2], 0), &mic, "clean mic left");
    assert_tone(&channel(&decoded[2], 1), &mic, "clean mic right");
    let polarity = rms(&channel(&decoded[1], 0)) / rms(&channel(&decoded[1], 1));
    assert!(
        (1.6..2.4).contains(&polarity),
        "the clean stem keeps both channels instead of collapsing them ({polarity})"
    );
}

#[test]
fn a_source_failure_and_an_encoder_rejection_are_terminal_and_clean_up_the_pipeline() {
    // A source failure while reading the tail keeps its code and closes every session.
    let mut backend = FakeBackend::default();
    backend.drain_script("a", vec![Err(source_lost("The drain failed."))]);
    let observed = Rc::clone(&backend.observed);
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::SeparateOnly),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();
    encoded.request_drain().unwrap();
    let error = encoded.poll_drain().unwrap_err();
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert_eq!(error.message, "The drain failed.");
    assert_eq!(
        encoded.failure().map(|stored| stored.code),
        Some(ErrorCode::SourceLost)
    );
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);
    // Terminal and sticky, and never a completed drain.
    assert_eq!(encoded.poll_drain().unwrap_err(), error);
    assert_eq!(encoded.poll(MediaTimestamp(0)).unwrap_err(), error);
    assert_eq!(encoded.request_drain().unwrap_err(), error);
    assert_eq!(encoded.stop().unwrap_err(), error);

    // An encoder rejection is terminal too: the encoders must share the pipeline's
    // origin, and a mismatched one rejects the quantum's sample timeline.
    let mut backend = FakeBackend::default();
    backend.script(
        "a",
        vec![Ok(Some(mono_block("a", 0, BLOCK as u64, 1_000.0, 0.25)))],
    );
    let observed = Rc::clone(&backend.observed);
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN + 1)).unwrap();
    let error = encoded.poll(MediaTimestamp(0)).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("sample timeline"));
    // The pipeline was healthy and was still closed by the encoder failure.
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
    assert_eq!(encoded.poll(MediaTimestamp(0)).unwrap_err(), error);
    assert_eq!(encoded.poll_drain().unwrap_err(), error);

    // A stop during a drain is the existing abort: cleanup runs and the flow never
    // claims to be drained.
    let mut backend = FakeBackend::default();
    backend.drain_script("a", vec![Ok(PcmDrainPoll::Pending)]);
    let observed = Rc::clone(&backend.observed);
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();
    encoded.request_drain().unwrap();
    assert!(matches!(
        encoded.poll_drain().unwrap(),
        EncodedAudioDrainPoll::Pending
    ));
    encoded.stop().unwrap();
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
    assert_eq!(
        encoded.poll_drain().unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert_eq!(
        encoded.poll(MediaTimestamp(0)).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert!(
        encoded.failure().is_none(),
        "an abort is not a stored failure"
    );

    // Dropping a draining pipeline runs the same abort cleanup.
    let mut backend = FakeBackend::default();
    backend.drain_script("a", vec![Ok(PcmDrainPoll::Pending)]);
    let observed = Rc::clone(&backend.observed);
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();
    encoded.request_drain().unwrap();
    drop(encoded);
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
}

#[test]
fn an_ignored_plan_builds_no_encoder_and_completes_its_drain_once() {
    let mut backend = FakeBackend::default();
    let observed = Rc::clone(&backend.observed);
    let pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("ignored", AudioRoute::Ignore),
            route("also-ignored", AudioRoute::Ignore),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(ORIGIN),
    )
    .unwrap();
    let mut encoded = EncodedAudioPipeline::new(pipeline, MediaTimestamp(ORIGIN)).unwrap();

    // No planned track means no encoder, no native session and no packet.
    assert!(encoded.tracks().is_empty());
    assert_eq!(encoded.pre_skip(0), None);
    assert!(observed.borrow().opened.is_empty());
    assert!(encoded.poll(MediaTimestamp(u64::MAX)).unwrap().is_empty());

    encoded.request_drain().unwrap();
    let finish = match encoded.poll_drain().unwrap() {
        EncodedAudioDrainPoll::Finished(finish) => finish,
        other => panic!("expected the one-shot finish report, got {other:?}"),
    };
    assert!(finish.packets.is_empty());
    assert!(finish.tracks.is_empty());
    assert!(matches!(
        encoded.poll_drain().unwrap(),
        EncodedAudioDrainPoll::Drained
    ));
    assert!(observed.borrow().opened.is_empty());
}
