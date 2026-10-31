use frame_audio::{
    AlignedPcmProcessor, AudioSession, DrainableAudioSession, NormalizingAudioSession, PcmBlock,
    PcmDrainPoll,
};
use frame_clock::MediaTimestamp;
use frame_contracts::*;
use std::{
    collections::VecDeque,
    num::{NonZeroU16, NonZeroU32},
};

fn block(id: &str, time: u64, rate: u32, samples: Vec<f32>) -> PcmBlock {
    PcmBlock {
        source_id: AudioSourceId(id.into()),
        timestamp: MediaTimestamp(time),
        sample_rate: NonZeroU32::new(rate).unwrap(),
        channels: NonZeroU16::new(2).unwrap(),
        samples,
    }
}
fn route(id: &str, controls: AudioSourceControls) -> AudioRouteSelection {
    AudioRouteSelection {
        source_id: AudioSourceId(id.into()),
        route: AudioRoute::MainMixAndSeparate,
        muted: false,
        controls,
    }
}
fn push(p: &mut AlignedPcmProcessor, b: PcmBlock) {
    if let Err((e, _)) = p.push(b) {
        panic!("{e:?}");
    }
}

#[test]
fn independent_source_gain_balance_stems_and_live_mute_have_real_samples() {
    let routes = [
        route(
            "desktop",
            AudioSourceControls {
                gain_millidb: -6021,
                balance: 100,
                ..Default::default()
            },
        ),
        route("mic", Default::default()),
    ];
    let mut p = AlignedPcmProcessor::new(&routes, MediaTimestamp(0)).unwrap();
    push(&mut p, block("desktop", 0, 48000, [0.4, 0.2].repeat(960)));
    push(&mut p, block("mic", 0, 48000, [0.1, -0.1].repeat(960)));
    let q = p.poll(MediaTimestamp(0)).unwrap().unwrap();
    assert_eq!(q.tracks.len(), 3);
    for frame in q.tracks[0].samples.chunks_exact(2) {
        assert!((frame[0] - 0.1).abs() < 1e-6);
        assert!(frame[1].abs() < 1e-5);
    }
    for frame in q.tracks[1].samples.chunks_exact(2) {
        assert_eq!(frame[0], 0.0);
        assert!((frame[1] - 0.1).abs() < 1e-5);
    }
    assert_eq!(q.tracks[2].samples, [0.1, -0.1].repeat(480));
    p.set_muted(&AudioSourceId("desktop".into()), true).unwrap();
    let q = p.poll(MediaTimestamp(0)).unwrap().unwrap();
    assert_eq!(q.tracks[0].samples, q.tracks[2].samples);
    assert!(q.tracks[1].samples.iter().all(|s| *s == 0.0));
}

#[test]
fn invalid_processing_is_refused_before_any_pcm_is_accepted() {
    for controls in [
        AudioSourceControls {
            gain_millidb: 24001,
            ..Default::default()
        },
        AudioSourceControls {
            balance: -101,
            ..Default::default()
        },
    ] {
        assert!(AlignedPcmProcessor::new(&[route("mic", controls)], MediaTimestamp(0)).is_err());
    }
}

struct Sparse {
    blocks: VecDeque<PcmBlock>,
    sparse: bool,
}
impl AudioSession for Sparse {
    fn allows_silent_gaps(&self) -> bool {
        self.sparse
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        Ok(self.blocks.pop_front())
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
}
impl DrainableAudioSession for Sparse {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        Ok(self
            .blocks
            .pop_front()
            .map_or(PcmDrainPoll::Drained, PcmDrainPoll::Block))
    }
}

#[test]
fn sparse_loopback_resumes_after_silence_at_native_time_without_reconnect() {
    for rate in [44100, 48000, 96000] {
        let samples = [0.2, -0.2].repeat(rate as usize / 10);
        let source = Sparse {
            sparse: true,
            blocks: [
                block("desktop", 0, rate, samples.clone()),
                block("desktop", 500_000_000, rate, samples),
            ]
            .into(),
        };
        let mut session = NormalizingAudioSession::new(source, AudioSourceId("desktop".into()));
        let mut blocks = Vec::new();
        while let Some(b) = session.try_pcm().unwrap() {
            blocks.push(b);
        }
        session.request_drain().unwrap();
        loop {
            match session.poll_drain().unwrap() {
                PcmDrainPoll::Block(b) => blocks.push(b),
                PcmDrainPoll::Drained => break,
                PcmDrainPoll::Pending => {}
            }
        }
        assert_eq!(
            blocks.iter().map(|b| b.samples.len() / 2).sum::<usize>(),
            9600,
            "rate {rate}"
        );
        assert!(
            blocks.iter().any(|b| b.timestamp.0 == 500_000_000),
            "rate {rate}"
        );
        assert!(
            blocks
                .iter()
                .all(|b| b.timestamp.0 < 100_000_000 || b.timestamp.0 >= 500_000_000)
        );
    }
}

#[test]
fn microphone_discontinuity_remains_an_explicit_error() {
    let source = Sparse {
        sparse: false,
        blocks: [
            block("mic", 0, 48000, vec![0.1; 960]),
            block("mic", 500_000_000, 48000, vec![0.1; 960]),
        ]
        .into(),
    };
    let mut s = NormalizingAudioSession::new(source, AudioSourceId("mic".into()));
    assert!(s.try_pcm().unwrap().is_some());
    match s.try_pcm() {
        Err(e) => assert_eq!(e.code, ErrorCode::SourceLost),
        _ => panic!("microphone gap must fail"),
    }
}

struct Loss {
    fail: bool,
}
impl AudioSession for Loss {
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if self.fail {
            Err(RecorderError {
                code: ErrorCode::SourceLost,
                message: "Device disappeared".into(),
                recovery: RecoveryStatus::NoRecording,
                action: "Reconnect".into(),
            })
        } else {
            Ok(None)
        }
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
}
struct LossBackend;
impl frame_audio::AudioBackend for LossBackend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        Ok(Vec::new())
    }
}
impl frame_audio::AudioCaptureBackend for LossBackend {
    type Session = Loss;
    fn start_audio(
        &mut self,
        _: &AudioSourceId,
        _: frame_audio::AudioContext,
    ) -> Result<Loss, RecorderError> {
        Ok(Loss { fail: true })
    }
    fn reopen_audio_factory(&self) -> Option<frame_audio::AudioReopenFactory<Loss>> {
        Some(|_, _| Ok(Loss { fail: false }))
    }
}

#[test]
fn each_route_applies_its_own_reconnect_policy_without_changing_the_process_default() {
    use frame_audio::{AudioContinuityPolicy, AudioPipeline, NormalizingBackend, SourceLifecycle};
    let routes = [
        route(
            "a",
            AudioSourceControls {
                reconnect: AudioReconnectPolicy::Silence,
                ..Default::default()
            },
        ),
        route(
            "b",
            AudioSourceControls {
                reconnect: AudioReconnectPolicy::ExactDevice,
                ..Default::default()
            },
        ),
    ];
    let mut backend =
        NormalizingBackend::with_policy(LossBackend, AudioContinuityPolicy::default());
    let mut pipeline = AudioPipeline::start(
        &mut backend,
        &routes,
        std::sync::Arc::new(frame_clock::MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap();
    pipeline.poll_sources().unwrap();
    let state: Vec<_> = pipeline.source_lifecycle().collect();
    assert!(matches!(
        state[0].state,
        SourceLifecycle::Ended { generation: 0 }
    ));
    assert!(matches!(
        state[1].state,
        SourceLifecycle::Reconnecting {
            generation: 0,
            attempts: 0
        }
    ));
    pipeline.poll_sources().unwrap();
    let state: Vec<_> = pipeline.source_lifecycle().collect();
    assert!(matches!(
        state[0].state,
        SourceLifecycle::Ended { generation: 0 }
    ));
    assert!(matches!(
        state[1].state,
        SourceLifecycle::Connected { generation: 1 }
    ));
    assert_eq!(pipeline.tracks().len(), 3);
    pipeline.stop().unwrap();
}
