// SPDX-License-Identifier: GPL-3.0-or-later
use frame_audio::{
    AudioBackend, AudioCaptureBackend, AudioContext, AudioPipeline, AudioSession,
    DrainableAudioSession, NormalizingBackend, PcmBlock, PcmDrainPoll,
};
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{
    AudioRoute, AudioRouteSelection, AudioSource, AudioSourceId, ErrorCode, RecorderError,
    RecoveryStatus,
};
use frame_encode::{
    audio_pipeline::{EncodedAudioDrainPoll, EncodedAudioPipeline},
    opus::OpusTrackEncoder,
};
use std::{
    num::{NonZeroU16, NonZeroU32},
    sync::Arc,
};

struct Backend {
    rate: u32,
    lost: bool,
}
struct Session {
    id: AudioSourceId,
    frame: u64,
    rate: u32,
    lost: bool,
}
impl AudioBackend for Backend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        Ok(vec![])
    }
}
impl AudioCaptureBackend for Backend {
    type Session = Session;
    fn start_audio(
        &mut self,
        id: &AudioSourceId,
        _: AudioContext,
    ) -> Result<Session, RecorderError> {
        Ok(Session {
            id: id.clone(),
            frame: 0,
            rate: self.rate,
            lost: self.lost && id.0 == "app",
        })
    }
}
impl AudioSession for Session {
    fn pcm_channels(&self) -> NonZeroU16 {
        NonZeroU16::new(1).unwrap()
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if self.lost {
            return Err(RecorderError {
                code: ErrorCode::SourceLost,
                message: "Application exited".into(),
                recovery: RecoveryStatus::NoRecording,
                action: String::new(),
            });
        }
        let count = (self.rate / 100) as usize;
        let timestamp = MediaTimestamp(self.frame * 1_000_000_000 / u64::from(self.rate));
        self.frame += count as u64;
        let value = if self.id.0 == "app" { 0.25 } else { 0.125 };
        Ok(Some(PcmBlock {
            source_id: self.id.clone(),
            timestamp,
            sample_rate: NonZeroU32::new(self.rate).unwrap(),
            channels: self.pcm_channels(),
            samples: vec![value; count],
        }))
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
}
impl DrainableAudioSession for Session {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        Ok(PcmDrainPoll::Drained)
    }
}
fn route(id: &str, route: AudioRoute) -> AudioRouteSelection {
    AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId(id.into()),
        route,
        muted: false,
    }
}
fn start(
    routes: &[AudioRouteSelection],
    rate: u32,
    lost: bool,
) -> AudioPipeline<frame_audio::RecoveringAudioSession<Session>> {
    AudioPipeline::start(
        &mut NormalizingBackend::with_policy(Backend { rate, lost }, Default::default()),
        routes,
        Arc::new(MasterClock::default()),
        MediaTimestamp(0),
    )
    .unwrap()
}
#[test]
fn mute_is_independent_and_clean_tracks_keep_mono_identity_and_time() {
    let mut audio = start(
        &[
            route("app", AudioRoute::MainMixAndSeparate),
            route("mic", AudioRoute::MainMixAndSeparate),
        ],
        48000,
        false,
    );
    let plan = audio.tracks().to_vec();
    let q = audio.poll(MediaTimestamp(0)).unwrap().unwrap();
    assert_eq!(q.tracks[0].samples, vec![0.375; 960]);
    assert_eq!(q.tracks[1].channels.get(), 1);
    assert_eq!(q.tracks[1].samples, vec![0.25; 480]);
    audio.set_muted(&AudioSourceId("app".into()), true).unwrap();
    let q = audio.poll(MediaTimestamp(0)).unwrap().unwrap();
    assert_eq!(q.tracks[0].samples, vec![0.125; 960]);
    assert_eq!(q.tracks[1].samples, vec![0.0; 480]);
    assert_eq!(q.tracks[2].samples, vec![0.125; 480]);
    assert_eq!(q.tracks[1].timestamp.0, 10_000_000);
    assert_eq!(audio.tracks(), plan);
    audio
        .set_muted(&AudioSourceId("app".into()), false)
        .unwrap();
    assert_eq!(
        audio.poll(MediaTimestamp(0)).unwrap().unwrap().tracks[1].samples,
        vec![0.25; 480]
    );
}
#[test]
fn many_independent_tracks_encode_and_drain_with_real_mono_opus() {
    let routes: Vec<_> = (0..24)
        .map(|i| route(&format!("mic{i:02}"), AudioRoute::SeparateOnly))
        .collect();
    let mut audio =
        EncodedAudioPipeline::new(start(&routes, 48000, false), MediaTimestamp(0)).unwrap();
    assert_eq!(audio.tracks().len(), 24);
    assert!(audio.poll(MediaTimestamp(0)).unwrap().is_empty());
    let packets = audio.poll(MediaTimestamp(0)).unwrap();
    assert_eq!(packets.len(), 24);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.track_id, i as u32);
        let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
        assert_eq!(
            decoder
                .decode_float(&packet.data, &mut [0.0; 960], false)
                .unwrap(),
            960
        );
    }
    audio.request_drain().unwrap();
    let mut finished = false;
    for _ in 0..8 {
        if let EncodedAudioDrainPoll::Finished(finish) = audio.poll_drain().unwrap() {
            assert_eq!(finish.tracks.len(), 24);
            assert!(finish.tracks.iter().all(|track| track.input_frames == 960));
            finished = true;
            break;
        }
    }
    assert!(finished);
    assert!(
        OpusTrackEncoder::with_channels(0, MediaTimestamp(0), NonZeroU16::new(1).unwrap()).is_ok()
    );
}
#[test]
fn application_exit_preserves_surviving_capture_and_resampling_keeps_mono() {
    let mut audio = start(
        &[
            route("app", AudioRoute::MainMixAndSeparate),
            route("mic", AudioRoute::SeparateOnly),
        ],
        48000,
        true,
    );
    let q = audio.poll(MediaTimestamp(200_000_000)).unwrap().unwrap();
    assert!(q.tracks[0].samples.iter().all(|s| *s == 0.0));
    assert_eq!(q.tracks[2].samples, vec![0.125; 480]);
    assert!(matches!(
        audio.source_lifecycle().next().unwrap().state,
        frame_audio::SourceLifecycle::Ended { .. }
    ));
    let mut audio = start(&[route("mic", AudioRoute::SeparateOnly)], 44100, false);
    let q = (0..8)
        .find_map(|_| audio.poll(MediaTimestamp(0)).unwrap())
        .unwrap();
    assert_eq!(q.tracks[0].channels.get(), 1);
    assert_eq!(q.tracks[0].samples.len(), 480);
    assert_eq!(q.tracks[0].timestamp.0, 0);
}
