// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Live pipeline tests against a scripted in-process session double.

use frame_audio::{
    AudioBackend, AudioCaptureBackend, AudioContext, AudioPipeline, AudioSession,
    DrainableAudioSession, PcmBlock, PcmDrainPoll, PcmQuantum, PipelineDrainPoll,
};
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline};
use frame_contracts::{
    AudioRoute, AudioRouteSelection, AudioSource, AudioSourceId, ErrorCode, RecorderError,
    RecoveryStatus,
};
use std::{
    cell::{Ref, RefCell},
    collections::VecDeque,
    num::{NonZeroU16, NonZeroU32},
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Observed {
    device_replacement: bool,
    /// Source IDs passed to `start_audio`, in call order.
    opened: Vec<String>,
    /// Source IDs whose session `stop` returned, in call order.
    stopped: Vec<String>,
    /// Source IDs whose session was dropped, in call order.
    dropped: Vec<String>,
    /// The exact clock `Arc` handed to each opened session.
    clocks: Vec<Arc<MasterClock>>,
    /// The cancellation token handed to each opened session.
    cancellations: Vec<frame_audio::AudioCancellation>,
    /// One entry per `try_pcm` call, in order.
    reads: Vec<String>,
    /// One entry per `request_drain` call, in order.
    drain_requests: Vec<String>,
    /// One entry per `poll_drain` call, in order.
    drain_polls: Vec<String>,
}

#[test]
fn hot_swap_preserves_tracks_clock_and_clips_only_old_device_overlap() {
    let mut backend = FakeBackend::default();
    backend.observed.borrow_mut().device_replacement = true;
    backend.script("a", vec![Ok(Some(block("a", 0, 2, vec![0.1; 960])))]);
    backend.script("b", vec![Ok(Some(block("b", 240, 2, vec![0.2; 1440])))]);
    let clock = Arc::new(MasterClock::default());
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMixAndSeparate)],
        Arc::clone(&clock),
        MediaTimestamp(0),
    )
    .unwrap();
    let tracks = pipeline.tracks().to_vec();
    pipeline.poll_sources().unwrap();
    pipeline
        .replace_source_device(
            &AudioSourceId("a".into()),
            &AudioSourceId("b".into()),
            &mut backend,
        )
        .unwrap();
    assert_eq!(pipeline.tracks(), tracks);
    assert!(
        backend
            .observed()
            .clocks
            .iter()
            .all(|value| Arc::ptr_eq(value, &clock))
    );
    let state = pipeline.source_lifecycle().next().unwrap();
    assert_eq!(state.source_id.0, "a");
    assert_eq!(state.capture_source_id.0, "b");
    assert_eq!(state.replacements, 1);
    pipeline.poll_sources().unwrap();
    let first = pipeline.poll(MediaTimestamp(20_000_000)).unwrap().unwrap();
    let next = pipeline.poll(MediaTimestamp(20_000_000)).unwrap().unwrap();
    assert_eq!(first.tracks[0].timestamp.0, 0);
    assert_eq!(next.tracks[0].timestamp.0, 10_000_000);
    assert!(
        next.tracks
            .iter()
            .all(|track| track.samples.iter().all(|sample| *sample == 0.2))
    );
    pipeline.request_drain().unwrap();
    assert!(
        pipeline
            .replace_source_device(
                &AudioSourceId("a".into()),
                &AudioSourceId("b".into()),
                &mut backend
            )
            .is_err()
    );
}

#[test]
fn reopened_generation_clips_old_pcm_and_keeps_generation_after_stop() {
    let mut backend = FakeBackend::default();
    backend.script(
        "a",
        vec![
            Ok(Some(block("a", 0, 2, vec![0.1; 960]))),
            Ok(Some(block("a", 240, 2, vec![0.2; 1440]))),
        ],
    );
    backend.generations.push(("a".into(), vec![0, 1]));
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMixAndSeparate)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    let tracks = pipeline.tracks().to_vec();
    let first = pipeline.poll(MediaTimestamp(20_000_000)).unwrap().unwrap();
    let next = pipeline.poll(MediaTimestamp(20_000_000)).unwrap().unwrap();
    assert_eq!(first.tracks[0].timestamp.0, 0);
    assert_eq!(next.tracks[0].timestamp.0, 10_000_000);
    assert!(
        next.tracks
            .iter()
            .all(|track| track.samples == vec![0.2; 960])
    );
    assert_eq!(pipeline.tracks(), tracks);
    let snapshot = pipeline.source_lifecycle().next().unwrap();
    assert_eq!(snapshot.continuity_clipped_frames, 240);
    assert_eq!(snapshot.replacements, 0);
    assert_eq!(
        snapshot.state,
        frame_audio::SourceLifecycle::Connected { generation: 1 }
    );
    pipeline.stop().unwrap();
    assert_eq!(
        pipeline.source_lifecycle().next().unwrap().state,
        frame_audio::SourceLifecycle::Ended { generation: 1 }
    );
}

#[test]
fn reopened_generation_cannot_replace_published_gap_quanta() {
    for paused in [false, true] {
        let mut backend = FakeBackend::default();
        backend.script(
            "a",
            vec![
                Ok(Some(block("a", 0, 2, vec![0.1; 960]))),
                Ok(None),
                Ok(Some(block(
                    "a",
                    if paused { 5520 } else { 720 },
                    2,
                    vec![0.2; 1440],
                ))),
            ],
        );
        backend.generations.push(("a".into(), vec![0, 0, 1]));
        let mut pipeline = AudioPipeline::start(
            &mut backend,
            &[route("a", AudioRoute::MainMixAndSeparate)],
            Arc::new(MasterClock::default()),
            MediaTimestamp(0),
        )
        .unwrap();
        assert_eq!(
            pipeline
                .poll(MediaTimestamp(10_000_000))
                .unwrap()
                .unwrap()
                .tracks[0]
                .timestamp
                .0,
            0
        );
        if paused {
            let timeline = Arc::new(Mutex::new(PauseTimeline::default()));
            timeline
                .lock()
                .unwrap()
                .pause(MediaTimestamp(10_000_000))
                .unwrap();
            timeline
                .lock()
                .unwrap()
                .resume(MediaTimestamp(110_000_000))
                .unwrap();
            pipeline.set_pause_timeline(timeline);
        }
        let gap = pipeline.poll(MediaTimestamp(200_000_000)).unwrap().unwrap();
        assert_eq!(gap.tracks[0].timestamp.0, 10_000_000);
        assert_eq!(gap.gaps[0].missing_frames, 480);
        assert!(
            gap.tracks
                .iter()
                .all(|track| track.samples == vec![0.0; 960])
        );
        let recovered = pipeline.poll(MediaTimestamp(200_000_000)).unwrap().unwrap();
        assert_eq!(recovered.tracks[0].timestamp.0, 20_000_000);
        assert!(
            recovered
                .tracks
                .iter()
                .all(|track| track.samples == vec![0.2; 960])
        );
        assert!(recovered.gaps.is_empty());
        assert_eq!(
            pipeline
                .source_lifecycle()
                .next()
                .unwrap()
                .continuity_clipped_frames,
            240
        );
        assert!(pipeline.failure().is_none());
    }
}

#[test]
fn replacement_stop_failure_keeps_group_terminal_even_with_source_lost_code() {
    let mut backend = FakeBackend::default();
    backend.observed.borrow_mut().device_replacement = true;
    backend.fail_stop = Some("a".into());
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    let error = pipeline
        .replace_source_device(
            &AudioSourceId("a".into()),
            &AudioSourceId("b".into()),
            &mut backend,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert!(error.message.contains("refused to stop"));
    assert_eq!(pipeline.failure(), Some(&error));
    assert_eq!(pipeline.poll_sources().unwrap_err(), error);
    assert!(backend.observed().stopped.iter().any(|id| id == "b"));
    assert!(backend.observed().dropped.iter().any(|id| id == "b"));
}

#[test]
fn pause_clips_pcm_before_and_after_boundary_then_drains_while_paused() {
    let mut backend = FakeBackend::default();
    backend.script(
        "a",
        vec![
            Ok(Some(block("a", 0, 2, vec![0.1; 960]))),
            Ok(Some(block("a", 480, 2, vec![0.2; 960]))),
            Ok(Some(block("a", 960, 2, vec![0.3; 960]))),
            Ok(Some(block("a", 5520, 2, vec![0.4; 960]))),
        ],
    );
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    let timeline = Arc::new(Mutex::new(PauseTimeline::default()));
    pipeline.set_pause_timeline(Arc::clone(&timeline));
    let first = pipeline.poll(MediaTimestamp(10_000_000)).unwrap().unwrap();
    assert_eq!(first.tracks[0].samples.len(), 960);
    timeline
        .lock()
        .unwrap()
        .pause(MediaTimestamp(15_000_000))
        .unwrap();
    assert_eq!(pipeline.poll_sources().unwrap(), 1);
    assert_eq!(pipeline.poll_sources().unwrap(), 1);
    timeline
        .lock()
        .unwrap()
        .resume(MediaTimestamp(115_000_000))
        .unwrap();
    assert_eq!(pipeline.poll_sources().unwrap(), 1);
    let resumed = pipeline.poll(MediaTimestamp(30_000_000)).unwrap().unwrap();
    assert_eq!(resumed.tracks[0].timestamp, MediaTimestamp(10_000_000));
    assert_eq!(&resumed.tracks[0].samples[..480], &vec![0.2; 480]);
    assert_eq!(&resumed.tracks[0].samples[480..], &vec![0.4; 480]);
    timeline
        .lock()
        .unwrap()
        .pause(MediaTimestamp(130_000_000))
        .unwrap();
    pipeline.request_drain().unwrap();
    let mut drained = false;
    for _ in 0..8 {
        if pipeline.poll_drain().unwrap() == PipelineDrainPoll::Drained {
            drained = true;
            break;
        }
    }
    assert!(drained, "Stop while paused drains the accepted PCM tail");
}

/// One scripted `try_pcm` result.
type Step = Result<Option<PcmBlock>, RecorderError>;
/// One scripted `poll_drain` result.
type DrainStep = Result<PcmDrainPoll, RecorderError>;
/// A contiguous run of scripted results for one source.
type Script = Vec<Step>;

struct FakeSession {
    id: String,
    observed: Rc<RefCell<Observed>>,
    script: VecDeque<Step>,
    drain_script: VecDeque<DrainStep>,
    fail_stop: bool,
    fail_drain_request: bool,
    generation: u64,
    generations: VecDeque<u64>,
}

impl AudioSession for FakeSession {
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        self.observed.borrow_mut().reads.push(self.id.clone());
        if let Some(generation) = self.generations.pop_front() {
            self.generation = generation;
        }
        self.script.pop_front().unwrap_or(Ok(None))
    }

    fn lifecycle(&self) -> frame_audio::SourceLifecycle {
        frame_audio::SourceLifecycle::Connected {
            generation: self.generation,
        }
    }

    fn stop(&mut self) -> Result<(), RecorderError> {
        assert!(
            self.observed.borrow().device_replacement
                || self
                    .observed
                    .borrow()
                    .cancellations
                    .iter()
                    .all(|token| token.is_cancelled()),
            "all workers must be cancelled before the first potentially blocking join"
        );
        self.observed.borrow_mut().stopped.push(self.id.clone());
        if self.fail_stop {
            return Err(source_lost("The fake session refused to stop."));
        }
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
        // A drain never cancels: the token stays untouched until stop or Drop.
        assert!(
            self.observed.borrow().device_replacement
                || !self
                    .observed
                    .borrow()
                    .cancellations
                    .iter()
                    .any(|token| token.is_cancelled()),
            "requesting a drain must not cancel any worker"
        );
        self.observed
            .borrow_mut()
            .drain_requests
            .push(self.id.clone());
        if self.fail_drain_request {
            return Err(source_lost("The fake session refused to drain."));
        }
        Ok(())
    }

    /// An exhausted drain script is a completed source, like a real finished worker.
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        self.observed.borrow_mut().drain_polls.push(self.id.clone());
        self.drain_script
            .pop_front()
            .unwrap_or(Ok(PcmDrainPoll::Drained))
    }
}

#[derive(Default)]
struct FakeBackend {
    observed: Rc<RefCell<Observed>>,
    scripts: Vec<(String, Script)>,
    drain_scripts: Vec<(String, Vec<DrainStep>)>,
    fail_open: Option<String>,
    fail_stop: Option<String>,
    fail_drain_request: Option<String>,
    generations: Vec<(String, Vec<u64>)>,
}

impl FakeBackend {
    fn script(&mut self, id: &str, results: Script) {
        self.scripts.push((id.to_string(), results));
    }

    fn drain_script(&mut self, id: &str, results: Vec<DrainStep>) {
        self.drain_scripts.push((id.to_string(), results));
    }

    fn observed(&self) -> Ref<'_, Observed> {
        self.observed.borrow()
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
        context: AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        if self.fail_open.as_deref() == Some(source.0.as_str()) {
            return Err(RecorderError::invalid(format!(
                "The fake backend cannot open {}.",
                source.0
            )));
        }
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
        let fail_stop = self.fail_stop.as_deref() == Some(source.0.as_str());
        let fail_drain_request = self.fail_drain_request.as_deref() == Some(source.0.as_str());
        let generations = self
            .generations
            .iter()
            .position(|(id, _)| id == &source.0)
            .map(|index| VecDeque::from(self.generations.remove(index).1))
            .unwrap_or_default();
        let mut observed = self.observed.borrow_mut();
        observed.opened.push(source.0.clone());
        observed.clocks.push(context.clock);
        observed.cancellations.push(context.cancellation);
        drop(observed);
        Ok(FakeSession {
            id: source.0.clone(),
            observed: Rc::clone(&self.observed),
            script,
            drain_script,
            fail_stop,
            fail_drain_request,
            generation: 0,
            generations,
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

fn block(id: &str, frame: u64, channels: u16, samples: Vec<f32>) -> PcmBlock {
    PcmBlock {
        source_id: AudioSourceId(id.into()),
        timestamp: MediaTimestamp(frame * 1_000_000_000 / 48_000),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
        channels: NonZeroU16::new(channels).unwrap(),
        samples,
    }
}

/// Shifts a block onto a nonzero session origin.
fn shifted(mut block: PcmBlock, offset: u64) -> PcmBlock {
    block.timestamp.0 += offset;
    block
}

fn source_lost(message: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message: message.to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Re-select the audio source and start a new session.".into(),
    }
}

/// The quantum of one drain step, or a panic naming the step that was not one.
fn quantum(poll: PipelineDrainPoll) -> PcmQuantum {
    match poll {
        PipelineDrainPoll::Quantum(quantum) => quantum,
        other => panic!("expected a drain quantum, got {other:?}"),
    }
}

#[test]
fn selected_routes_feed_main_and_clean_content_and_never_open_an_ignored_source() {
    let clock = Arc::new(MasterClock::default());
    let mut backend = FakeBackend::default();
    backend.script(
        "mic",
        vec![Ok(Some(shifted(
            block("mic", 0, 1, vec![0.25; 480]),
            1_000,
        )))],
    );
    backend.script(
        "app",
        vec![Ok(Some(shifted(
            block("app", 0, 2, [0.5, -0.25].repeat(480)),
            1_000,
        )))],
    );
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("ignored", AudioRoute::Ignore),
            route("app", AudioRoute::SeparateOnly),
            route("mic", AudioRoute::MainMixAndSeparate),
        ],
        Arc::clone(&clock),
        MediaTimestamp(1_000),
    )
    .unwrap();

    // Ignore is planned but never opened; opening order is the stable source-ID order.
    assert_eq!(observed.borrow().opened, vec!["app", "mic"]);
    // One externally supplied clock reaches every source; tokens stay independent.
    for seen in &observed.borrow().clocks {
        assert!(Arc::ptr_eq(seen, &clock));
    }
    let cancellations = observed.borrow().cancellations.clone();
    cancellations[0].cancel();
    assert!(cancellations[0].is_cancelled());
    assert!(!cancellations[1].is_cancelled());

    let quantum = pipeline.poll(MediaTimestamp(1_000)).unwrap().unwrap();
    assert_eq!(quantum.tracks.len(), 3);
    assert_eq!(quantum.tracks[0].samples, vec![0.25; 960]);
    assert_eq!(quantum.tracks[1].samples, [0.5, -0.25].repeat(480));
    assert_eq!(quantum.tracks[2].samples, vec![0.25; 960]);
    assert_eq!(quantum.tracks[0].timestamp, MediaTimestamp(1_000));
    assert!(quantum.gaps.is_empty());
    // At most one quantum per poll: the same media time yields nothing more.
    assert!(pipeline.poll(MediaTimestamp(1_000)).unwrap().is_none());
}

#[test]
fn a_failed_open_stops_drops_and_cancels_every_session_opened_before_it() {
    let clock = Arc::new(MasterClock::default());
    let mut backend = FakeBackend {
        fail_open: Some("b".into()),
        ..FakeBackend::default()
    };
    let observed = Rc::clone(&backend.observed);
    let error = AudioPipeline::<FakeSession>::start(
        &mut backend,
        &[
            route("b", AudioRoute::MainMix),
            route("a", AudioRoute::MainMix),
        ],
        clock,
        MediaTimestamp(0),
    )
    .err()
    .expect("the second open fails");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(error.message, "The fake backend cannot open b.");
    assert_eq!(observed.borrow().opened, vec!["a"]);
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
    assert!(observed.borrow().cancellations[0].is_cancelled());
}

#[test]
fn a_cleanup_failure_during_start_is_reported_beside_the_primary_error() {
    let mut backend = FakeBackend {
        fail_open: Some("b".into()),
        fail_stop: Some("a".into()),
        ..FakeBackend::default()
    };
    let error = AudioPipeline::<FakeSession>::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::MainMix),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .err()
    .expect("the second open fails");
    // The primary failure keeps its code; the cleanup failure is never discarded.
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("cannot open b"));
    assert!(error.message.contains("Cleanup also failed"));
    assert!(error.message.contains("refused to stop"));
    assert_eq!(backend.observed().stopped, vec!["a"]);
    assert_eq!(backend.observed().dropped, vec!["a"]);
}

#[test]
fn a_source_failure_is_terminal_sticky_and_closes_every_session() {
    let mut backend = FakeBackend::default();
    backend.script("a", vec![Err(source_lost("The device was lost."))]);
    backend.script("b", vec![Ok(Some(block("b", 0, 1, vec![0.0; 480])))]);
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMixAndSeparate),
            route("b", AudioRoute::MainMix),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();

    let error = pipeline.poll(MediaTimestamp(0)).unwrap_err();
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert_eq!(
        pipeline.failure().map(|stored| stored.code),
        Some(ErrorCode::SourceLost)
    );
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);

    // The stored failure is repeated without touching a closed source again.
    let reads = observed.borrow().reads.len();
    assert_eq!(pipeline.poll(MediaTimestamp(0)).unwrap_err(), error);
    assert_eq!(observed.borrow().reads.len(), reads);
    assert_eq!(pipeline.stop().unwrap_err(), error);
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
}

#[test]
fn stop_and_drop_close_every_session_exactly_once_and_poll_after_stop_is_explicit() {
    let mut backend = FakeBackend::default();
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::SeparateOnly),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();

    pipeline.stop().unwrap();
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);
    assert!(
        observed
            .borrow()
            .cancellations
            .iter()
            .all(frame_audio::AudioCancellation::is_cancelled)
    );
    // Repeated stop and poll never resurrect a stopped group.
    pipeline.stop().unwrap();
    assert_eq!(
        pipeline.poll(MediaTimestamp(0)).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    drop(pipeline);
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);

    // Dropping a running pipeline is itself a complete cleanup.
    let mut backend = FakeBackend::default();
    let observed = Rc::clone(&backend.observed);
    let _pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    drop(_pipeline);
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
}

#[test]
fn every_poll_reads_at_most_one_block_per_source_and_returns_at_most_one_quantum() {
    let mut backend = FakeBackend::default();
    for id in ["a", "b"] {
        backend.script(
            id,
            (0..3)
                .map(|index| Ok(Some(block(id, index * 480, 1, vec![0.5; 480]))))
                .collect(),
        );
    }
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::MainMix),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();

    for expected in [
        MediaTimestamp(0),
        MediaTimestamp(10_000_000),
        MediaTimestamp(20_000_000),
    ] {
        let quantum = pipeline.poll(MediaTimestamp(0)).unwrap().unwrap();
        assert_eq!(quantum.tracks[0].timestamp, expected);
        assert_eq!(quantum.tracks[0].samples, vec![1.0; 960]);
    }
    // One read per source per poll: a second read in one poll would show up here.
    assert_eq!(observed.borrow().reads, vec!["a", "b", "a", "b", "a", "b"]);
    // The exhausted scripts yield no further quantum instead of blocking or sleeping.
    assert!(pipeline.poll(MediaTimestamp(0)).unwrap().is_none());
}

#[test]
fn wrong_identity_and_unsupported_format_are_terminal_and_never_dropped_quietly() {
    // A block tagged as another configured source must not be filed under that source.
    let mut backend = FakeBackend::default();
    backend.script("a", vec![Ok(Some(block("b", 0, 1, vec![0.0; 480])))]);
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::MainMix),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    let error = pipeline.poll(MediaTimestamp(0)).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("tagged as b"));
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);

    // 44.1 kHz and three-channel blocks are refused by the frozen ADR 0009 policy.
    for (rate, channels) in [(44_100_u32, 1_u16), (48_000, 3)] {
        let mut backend = FakeBackend::default();
        let mut block = block("a", 0, channels, vec![0.0; 480 * usize::from(channels)]);
        block.sample_rate = NonZeroU32::new(rate).unwrap();
        backend.script("a", vec![Ok(Some(block))]);
        let observed = Rc::clone(&backend.observed);
        let mut pipeline = AudioPipeline::start(
            &mut backend,
            &[route("a", AudioRoute::MainMix)],
            Arc::new(MasterClock::default()),
            MediaTimestamp(0),
        )
        .unwrap();
        assert_eq!(
            pipeline.poll(MediaTimestamp(0)).unwrap_err().code,
            ErrorCode::NotImplemented
        );
        assert_eq!(observed.borrow().stopped, vec!["a"]);
        assert_eq!(observed.borrow().dropped, vec!["a"]);
    }
}

#[test]
fn frozen_route_validation_precedes_open_and_many_sources_are_supported() {
    let mut backend = FakeBackend::default();
    let observed = Rc::clone(&backend.observed);
    let duplicate = [
        route("a", AudioRoute::MainMix),
        route("a", AudioRoute::SeparateOnly),
    ];
    assert!(
        AudioPipeline::<FakeSession>::start(
            &mut backend,
            &duplicate,
            Arc::new(MasterClock::default()),
            MediaTimestamp(0),
        )
        .is_err()
    );
    let too_many: Vec<_> = (0..40)
        .map(|index| route(&format!("a{index}"), AudioRoute::MainMix))
        .collect();
    assert!(
        AudioPipeline::<FakeSession>::start(
            &mut backend,
            &too_many,
            Arc::new(MasterClock::default()),
            MediaTimestamp(0),
        )
        .is_ok()
    );
    assert_eq!(observed.borrow().opened.len(), 40);

    // An all-ignored plan opens nothing and still stops cleanly.
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("ignored", AudioRoute::Ignore),
            route("also-ignored", AudioRoute::Ignore),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    assert!(pipeline.tracks().is_empty());
    assert!(pipeline.poll(MediaTimestamp(u64::MAX)).unwrap().is_none());
    // An empty plan has no source to notify and no mixer deadline to wait for, so its
    // drain ends on the first poll without ever producing a quantum.
    pipeline.request_drain().unwrap();
    assert_eq!(pipeline.poll_drain().unwrap(), PipelineDrainPoll::Drained);
    pipeline.stop().unwrap();
    assert_eq!(observed.borrow().opened.len(), 40);
}

#[test]
fn cleanup_failure_stays_visible_on_repeated_stop_without_repeating_cleanup() {
    let mut backend = FakeBackend {
        fail_stop: Some("a".into()),
        ..FakeBackend::default()
    };
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::MainMix),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    let failure = pipeline.stop().unwrap_err();
    assert_eq!(pipeline.stop().unwrap_err(), failure);
    assert_eq!(pipeline.poll(MediaTimestamp(0)).unwrap_err(), failure);
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);
}

#[test]
fn drain_delivers_queued_audio_a_pending_step_and_the_short_final_piece() {
    let mut backend = FakeBackend::default();
    backend.drain_script(
        "a",
        vec![
            Ok(PcmDrainPoll::Block(block("a", 0, 1, vec![0.5; 480]))),
            // A worker between two native packets is not a verdict.
            Ok(PcmDrainPoll::Pending),
            Ok(PcmDrainPoll::Block(block("a", 480, 1, vec![0.25; 600]))),
            Ok(PcmDrainPoll::Drained),
        ],
    );
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();

    // Idempotent: a repeated request notifies no source twice and cancels nothing.
    pipeline.request_drain().unwrap();
    pipeline.request_drain().unwrap();
    assert_eq!(observed.borrow().drain_requests, vec!["a"]);
    assert!(!observed.borrow().cancellations[0].is_cancelled());
    assert!(observed.borrow().stopped.is_empty());
    assert!(observed.borrow().dropped.is_empty());

    // The live path is invalid while the drain owns the tail; the rejected poll
    // neither fails the group nor disturbs the drain or its sessions.
    let live = pipeline.poll(MediaTimestamp(0)).unwrap_err();
    assert_eq!(live.code, ErrorCode::InvalidTransition);
    assert!(pipeline.failure().is_none());
    assert!(observed.borrow().drain_polls.is_empty());
    assert!(observed.borrow().reads.is_empty());

    // The first published block becomes one ordinary quantum.
    let first = quantum(pipeline.poll_drain().unwrap());
    assert_eq!(first.tracks.len(), 1);
    assert_eq!(first.tracks[0].timestamp, MediaTimestamp(0));
    assert_eq!(first.tracks[0].samples, vec![0.5; 960]);
    assert!(first.gaps.is_empty());
    // One read per source per drain poll.
    assert_eq!(observed.borrow().drain_polls, vec!["a"]);

    // A `Pending` source withholds the next quantum rather than letting a wall clock
    // fill the range with silence, so the block queued behind it keeps its position.
    assert_eq!(pipeline.poll_drain().unwrap(), PipelineDrainPoll::Pending);
    assert_eq!(observed.borrow().drain_polls, vec!["a", "a"]);

    let second = quantum(pipeline.poll_drain().unwrap());
    assert_eq!(second.tracks[0].timestamp, MediaTimestamp(10_000_000));
    assert_eq!(second.tracks[0].samples, vec![0.25; 960]);
    // Still draining: the tail is not consumed and no session is closed yet.
    assert!(!observed.borrow().cancellations[0].is_cancelled());
    assert!(observed.borrow().stopped.is_empty());

    // The `Drained` verdict ends the source and finishes the mixer before the last,
    // shorter 120-frame piece is handed over.
    let tail = quantum(pipeline.poll_drain().unwrap());
    assert_eq!(tail.tracks[0].timestamp, MediaTimestamp(20_000_000));
    assert_eq!(tail.tracks[0].samples, vec![0.25; 240]);
    assert!(tail.gaps.is_empty());
    assert_eq!(observed.borrow().drain_polls, vec!["a"; 4]);
    // Mixer drained and every session closed: the verdict is now permanent.
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
    assert!(observed.borrow().cancellations[0].is_cancelled());
    assert_eq!(pipeline.poll_drain().unwrap(), PipelineDrainPoll::Drained);
    assert_eq!(pipeline.poll_drain().unwrap(), PipelineDrainPoll::Drained);
    // A completed drain never reads a closed source again.
    assert_eq!(observed.borrow().drain_polls, vec!["a"; 4]);
    assert_eq!(
        pipeline.poll(MediaTimestamp(0)).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
}

#[test]
fn a_drain_failure_keeps_its_first_code_closes_every_session_and_repeats() {
    // A failure while reading the tail is terminal for the whole group.
    let mut backend = FakeBackend::default();
    backend.drain_script("a", vec![Err(source_lost("The drain failed."))]);
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::SeparateOnly),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    pipeline.request_drain().unwrap();
    // Every source is notified, in stable source-ID order.
    assert_eq!(observed.borrow().drain_requests, vec!["a", "b"]);

    let error = pipeline.poll_drain().unwrap_err();
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert_eq!(error.message, "The drain failed.");
    assert_eq!(
        pipeline.failure().map(|stored| stored.code),
        Some(ErrorCode::SourceLost)
    );
    // The failure wins over the drain: every session is stopped and dropped.
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);

    // The stored failure repeats without reading a closed source again.
    let polls = observed.borrow().drain_polls.len();
    assert_eq!(pipeline.poll_drain().unwrap_err(), error);
    assert_eq!(observed.borrow().drain_polls.len(), polls);
    assert_eq!(pipeline.poll(MediaTimestamp(0)).unwrap_err(), error);
    assert_eq!(pipeline.request_drain().unwrap_err(), error);

    // A failed request is the same terminal failure: the remaining sources are still
    // notified before every session is closed and the first code is kept.
    let mut backend = FakeBackend {
        fail_drain_request: Some("a".into()),
        ..FakeBackend::default()
    };
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[
            route("a", AudioRoute::MainMix),
            route("b", AudioRoute::SeparateOnly),
        ],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    let error = pipeline.request_drain().unwrap_err();
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert_eq!(error.message, "The fake session refused to drain.");
    assert_eq!(observed.borrow().drain_requests, vec!["a", "b"]);
    assert_eq!(observed.borrow().stopped, vec!["a", "b"]);
    assert_eq!(observed.borrow().dropped, vec!["a", "b"]);
    assert_eq!(pipeline.poll_drain().unwrap_err(), error);
}

#[test]
fn an_aborted_drain_keeps_the_existing_stop_cleanup_and_never_reports_drained() {
    let mut backend = FakeBackend::default();
    backend.drain_script("a", vec![Ok(PcmDrainPoll::Pending)]);
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();

    pipeline.request_drain().unwrap();
    assert_eq!(pipeline.poll_drain().unwrap(), PipelineDrainPoll::Pending);
    // stop keeps its abort contract: cancel, stop, drop, and no Drained verdict.
    pipeline.stop().unwrap();
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
    assert!(observed.borrow().cancellations[0].is_cancelled());
    assert_eq!(
        pipeline.poll_drain().unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert_eq!(
        pipeline.request_drain().unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert!(pipeline.failure().is_none());

    // A drain that was never requested is invalid, and a dropped draining pipeline
    // runs the same abort cleanup.
    let mut backend = FakeBackend::default();
    let observed = Rc::clone(&backend.observed);
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &[route("a", AudioRoute::MainMix)],
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    assert_eq!(
        pipeline.poll_drain().unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    pipeline.request_drain().unwrap();
    drop(pipeline);
    assert_eq!(observed.borrow().stopped, vec!["a"]);
    assert_eq!(observed.borrow().dropped, vec!["a"]);
}
