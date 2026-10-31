//! Explicit idle audio test owner. It runs only on the recording control thread.
use std::sync::Arc;

use frame_audio::metering::MeterHub;
use frame_audio::{
    AudioBackend, AudioCancellation, AudioCaptureBackend, AudioContext, AudioSession,
};
use frame_clock::MasterClock;
use frame_contracts::{AudioSourceId, ErrorCode, RecorderError, RecoveryStatus};
use frame_windows::{PlatformBackend, WindowsAudioSession};

struct Source<S = WindowsAudioSession> {
    id: AudioSourceId,
    cancellation: AudioCancellation,
    session: S,
}

// Native source queues hold at most 32 PCM blocks. Drain a complete queued
// burst each control step; one block per step can fall behind WASAPI even when
// the loop requests a 1 ms park, because Windows scheduling is not a 1 ms clock.
const MAX_BLOCKS_PER_SOURCE_POLL: usize = 32;

fn poll_source<S: AudioSession>(
    source: &mut Source<S>,
    meter: &MeterHub,
    session_id: &str,
) -> Result<(), RecorderError> {
    for _ in 0..MAX_BLOCKS_PER_SOURCE_POLL {
        let Some(block) = source.session.try_pcm()? else {
            break;
        };
        if block.source_id != source.id {
            return Err(RecorderError::invalid(
                "Audio level source delivered PCM tagged with another source identity.",
            ));
        }
        meter.observe(session_id, &block.source_id, &block.samples);
    }
    Ok(())
}

pub struct AudioLevelTest {
    sources: Vec<Source>,
    meter: Arc<MeterHub>,
    session_id: String,
}

impl AudioLevelTest {
    pub fn start(
        ids: &[AudioSourceId],
        meter: Arc<MeterHub>,
        session_id: String,
    ) -> Result<Self, RecorderError> {
        if ids.is_empty() {
            return Err(RecorderError::invalid(
                "Select at least one audio source to test.",
            ));
        }
        let mut sorted = ids.to_vec();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != ids.len() {
            return Err(RecorderError::invalid(
                "The audio level test contains a duplicate source.",
            ));
        }
        frame_audio::AlignedPcmProcessor::new(
            &sorted
                .iter()
                .map(|id| frame_contracts::AudioRouteSelection {
                    controls: Default::default(),
                    source_id: id.clone(),
                    route: frame_contracts::AudioRoute::MainMix,
                    muted: false,
                })
                .collect::<Vec<_>>(),
            frame_clock::MediaTimestamp(0),
        )?;
        let available = PlatformBackend.enumerate_audio_sources()?;
        let routes: Vec<_> = sorted
            .iter()
            .map(|id| frame_contracts::AudioRouteSelection {
                source_id: id.clone(),
                route: frame_contracts::AudioRoute::MainMix,
                muted: false,
                controls: Default::default(),
            })
            .collect();
        frame_settings::audio_admission::validate_audio_selection(
            &routes,
            &available,
            frame_windows::windows_build_number()? >= 20348,
            true,
        )?;
        for id in &sorted {
            if !available.iter().any(|source| &source.id == id) {
                return Err(RecorderError {
                    code: ErrorCode::SourceLost,
                    message: format!("Selected audio source {} is no longer available.", id.0),
                    recovery: RecoveryStatus::NoRecording,
                    action: "Refresh audio sources and select an available source.".into(),
                });
            }
        }
        let clock = Arc::new(MasterClock::default());
        let mut backend = PlatformBackend;
        let mut sources: Vec<Source> = Vec::with_capacity(sorted.len());
        for id in &sorted {
            let cancellation = AudioCancellation::default();
            match backend.start_audio(
                id,
                AudioContext {
                    clock: Arc::clone(&clock),
                    cancellation: cancellation.clone(),
                },
            ) {
                Ok(session) => sources.push(Source {
                    id: id.clone(),
                    cancellation,
                    session,
                }),
                Err(error) => {
                    for source in &mut sources {
                        source.cancellation.cancel();
                        let _ = source.session.stop();
                    }
                    return Err(error);
                }
            }
        }
        meter.begin(session_id.clone(), &sorted);
        Ok(Self {
            sources,
            meter,
            session_id,
        })
    }

    /// Drain each source's queued burst, with a fixed bound and no blocking wait.
    pub fn poll(&mut self) -> Result<(), RecorderError> {
        for source in &mut self.sources {
            poll_source(source, &self.meter, &self.session_id)?;
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), RecorderError> {
        let mut first = None;
        for source in &mut self.sources {
            source.cancellation.cancel();
            if let Err(error) = source.session.stop() {
                first.get_or_insert(error);
            }
        }
        self.sources.clear();
        self.meter.end();
        first.map_or(Ok(()), Err)
    }
}

impl Drop for AudioLevelTest {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_audio::PcmBlock;
    use frame_clock::MediaTimestamp;
    use std::collections::VecDeque;

    struct QueuedSession {
        queue: VecDeque<PcmBlock>,
        always_ready: bool,
        calls: usize,
        failure: Option<RecorderError>,
    }
    fn block(id: &str) -> PcmBlock {
        PcmBlock {
            source_id: AudioSourceId(id.into()),
            timestamp: MediaTimestamp(0),
            sample_rate: 48_000.try_into().unwrap(),
            channels: 2.try_into().unwrap(),
            samples: vec![0.25; 960],
        }
    }
    impl AudioSession for QueuedSession {
        fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
            self.calls += 1;
            if let Some(error) = self.failure.take() {
                return Err(error);
            }
            Ok(self
                .queue
                .pop_front()
                .or_else(|| self.always_ready.then(|| block("mic"))))
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
    }
    fn source() -> Source<QueuedSession> {
        Source {
            id: AudioSourceId("mic".into()),
            cancellation: AudioCancellation::default(),
            session: QueuedSession {
                queue: VecDeque::new(),
                always_ready: false,
                calls: 0,
                failure: None,
            },
        }
    }
    #[test]
    fn scheduler_bursts_do_not_accumulate_in_the_level_test_queue() {
        let mut source = source();
        let meter = MeterHub::default();
        meter.begin("test".into(), std::slice::from_ref(&source.id));
        // Three native packets can arrive between delayed control-loop wakes.
        // The previous one-block poll leaves two per step and overflows at 32.
        for _ in 0..100 {
            source.session.queue.extend((0..3).map(|_| block("mic")));
            assert!(source.session.queue.len() <= 32);
            poll_source(&mut source, &meter, "test").unwrap();
            assert!(source.session.queue.is_empty());
        }
        assert_eq!(meter.snapshot().sources[0].peak, Some(0.25));
    }
    #[test]
    fn continuously_ready_source_cannot_starve_control_commands() {
        let mut source = source();
        source.session.always_ready = true;
        poll_source(&mut source, &MeterHub::default(), "test").unwrap();
        assert_eq!(source.session.calls, MAX_BLOCKS_PER_SOURCE_POLL);
    }
    #[test]
    fn foreign_source_block_is_rejected_before_metering() {
        let mut source = source();
        source.session.queue.push_back(block("foreign"));
        let meter = MeterHub::default();
        meter.begin("test".into(), std::slice::from_ref(&source.id));
        assert!(poll_source(&mut source, &meter, "test").is_err());
        assert_eq!(meter.snapshot().sources[0].peak, None);
    }
    #[test]
    fn native_failure_is_preserved_instead_of_a_passing_level_test() {
        let mut source = source();
        let error = RecorderError::invalid("Synthetic source failure");
        source.session.failure = Some(error.clone());
        assert_eq!(
            poll_source(&mut source, &MeterHub::default(), "test"),
            Err(error)
        );
    }
}
