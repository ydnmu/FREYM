//! Enumeration, timestamped PCM and deterministic route planning.
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{AudioRoute, AudioRouteSelection, AudioSource, AudioSourceId, RecorderError};
use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub mod metering;
mod mixer;
mod normalize;
mod pipeline;
mod policy;
mod reconnect;
pub use mixer::{AlignedPcmProcessor, GapCount, PcmQuantum, PushOutcome, TrackPcm};
pub use normalize::{
    ADAPTER_MEMORY_BUDGET, MAX_BLOCK_FRAMES, MAX_READY_BLOCKS, NORMALIZED_CHANNELS,
    NORMALIZED_RATE, NormalizeCounters, NormalizingAudioSession, NormalizingBackend,
    OUTPUT_BLOCK_FRAMES, SOURCE_INTERVAL_TOLERANCE_NS, SourceFormat,
};
pub use pipeline::{AudioPipeline, PipelineDrainPoll, SourceLifecycleSnapshot};
pub use policy::{AudioContinuityPolicy, DriftCompensationHealth};
pub use reconnect::{RecoveringAudioSession, SourceLifecycle};

pub trait AudioBackend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError>;
}

/// One-way cancellation for one source start/session. Allocate a fresh token
/// for each source so a failed microphone cannot cancel video or another source.
#[derive(Clone, Default)]
pub struct AudioCancellation(Arc<CancellationState>);
#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    parent: Option<Arc<CancellationState>>,
}
impl AudioCancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        let mut state = &self.0;
        loop {
            if state.cancelled.load(Ordering::Acquire) {
                return true;
            }
            match &state.parent {
                Some(parent) => state = parent,
                None => return false,
            }
        }
    }

    /// Native generation cleanup cancels only this child. Owner cancellation is
    /// visible to every generation before the group's first blocking join.
    pub fn child(&self) -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            parent: Some(Arc::clone(&self.0)),
        }))
    }
}

/// Inject the recording's existing clock; never create an epoch per source.
pub struct AudioContext {
    pub clock: Arc<MasterClock>,
    pub cancellation: AudioCancellation,
}

/// Native capture extension, separate from discovery-only backends. Implementing
/// this trait does not enable any recorder/IPC capability (ADR 0007).
pub trait AudioCaptureBackend: AudioBackend {
    type Session: AudioSession;

    /// Revalidate the complete selection before any session is opened.
    fn validate_capture_routes(
        &mut self,
        _routes: &[AudioRouteSelection],
    ) -> Result<(), RecorderError> {
        Ok(())
    }

    /// Called on a control thread. Resolve exactly this source; never substitute
    /// a default device or system mix. Return only after native readiness.
    fn start_audio(
        &mut self,
        source: &AudioSourceId,
        context: AudioContext,
    ) -> Result<Self::Session, RecorderError>;

    /// A backend may provide a control-thread factory for reopening the exact
    /// stable source. Stateless native backends can support it without retaining
    /// a borrowed backend beyond recording preparation.
    fn reopen_audio_factory(&self) -> Option<AudioReopenFactory<Self::Session>> {
        None
    }
}

pub type AudioReopenFactory<S> = fn(&AudioSourceId, AudioContext) -> Result<S, RecorderError>;

pub struct PcmBlock {
    pub source_id: AudioSourceId,
    pub timestamp: MediaTimestamp,
    pub sample_rate: std::num::NonZeroU32,
    pub channels: std::num::NonZeroU16,
    /// Interleaved f32 samples. Producers must cap block sizes and queue capacity.
    pub samples: Vec<f32>,
}

pub trait AudioSession {
    /// Endpoint loopback may omit silent intervals. Timestamps still locate real PCM.
    fn allows_silent_gaps(&self) -> bool {
        false
    }

    /// Frozen supported channel count available after native readiness.
    fn pcm_channels(&self) -> std::num::NonZeroU16 {
        std::num::NonZeroU16::new(2).unwrap()
    }

    /// Nonblocking dequeue. None means no ready block, including transient queue
    /// contention; sticky worker failures take precedence over buffered PCM.
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError>;
    /// Idempotent cancellation/cleanup/join on a control thread, not the UI or a
    /// media callback. Implementations also cancel and join on Drop. Owned PCM
    /// already handed to a consumer remains valid after stop.
    fn stop(&mut self) -> Result<(), RecorderError>;

    /// Release a source before a deliberate continuity transition. A backend
    /// may suppress only the exact capture failure it already reported; a new
    /// cleanup error must remain an error even when its code is `SourceLost`.
    /// The default is conservative and preserves every stop failure.
    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        self.stop()
    }

    /// Optional observation of raw source sample-clock speed. `None` means no
    /// sufficiently long measurement, never a claim of zero drift.
    fn clock_drift(&self) -> Option<frame_clock::DriftEstimate> {
        None
    }

    fn compensation_health(&self) -> Option<DriftCompensationHealth> {
        None
    }

    fn lifecycle(&self) -> SourceLifecycle {
        SourceLifecycle::Connected { generation: 0 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceClockHealth {
    pub source_id: AudioSourceId,
    pub drift: frame_clock::DriftEstimate,
}

/// One step of a graceful drain, in the same three states the accepted encoder
/// contract uses. Buffered PCM is never discarded by a drain.
pub enum PcmDrainPoll {
    /// One already-published block. It is returned exactly once, whichever poll
    /// method takes it.
    Block(PcmBlock),
    /// The worker has not finished, or the queue is not empty yet, or a lock is
    /// briefly held elsewhere. `Pending` never proves a completed drain and never
    /// means the source failed.
    Pending,
    /// The worker finished a requested graceful drain and every published block was
    /// returned. Repeated polls stay `Drained`; a sticky failure always wins.
    Drained,
}

/// Graceful end of capture, separate from cancellation/abort. There is deliberately
/// no default implementation: a session that cannot drain must not pretend to.
pub trait DrainableAudioSession: AudioSession {
    /// Asks the worker to stop taking new native packets. Idempotent and
    /// nonblocking: it never cancels, discards, joins or waits, and every block
    /// already published stays readable. Audio the OS has not handed over yet is
    /// not covered by any losslessness claim.
    fn request_drain(&mut self) -> Result<(), RecorderError>;

    /// Returns at most one already-published block, or `Pending` while a lock is
    /// contended or the drain is unfinished, or `Drained` only once the worker
    /// finished successfully and the queue is empty. A sticky failure takes
    /// precedence over buffered PCM. An aborted session returns InvalidTransition,
    /// never an indefinite Pending result.
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackKind {
    MainMix,
    Clean(AudioSourceId),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPlan {
    pub kind: TrackKind,
    pub sources: Vec<AudioSourceId>,
}

/// Main first (only when used), then clean stems sorted by stable source ID.
/// Each source contributes to the mix at most once. No DSP or encoding here.
pub fn plan_tracks(routes: &[AudioRouteSelection]) -> Result<Vec<TrackPlan>, RecorderError> {
    let mut seen = BTreeSet::new();
    let mut main = Vec::new();
    let mut separate = Vec::new();
    for selection in routes {
        if !seen.insert(selection.source_id.clone()) {
            return Err(RecorderError::invalid("Duplicate audio source route."));
        }
        if matches!(
            selection.route,
            AudioRoute::MainMix | AudioRoute::MainMixAndSeparate
        ) {
            main.push(selection.source_id.clone());
        }
        if matches!(
            selection.route,
            AudioRoute::SeparateOnly | AudioRoute::MainMixAndSeparate
        ) {
            separate.push(selection.source_id.clone());
        }
    }
    main.sort();
    separate.sort();
    let mut tracks = Vec::new();
    if !main.is_empty() {
        tracks.push(TrackPlan {
            kind: TrackKind::MainMix,
            sources: main,
        });
    }
    tracks.extend(separate.into_iter().map(|id| TrackPlan {
        kind: TrackKind::Clean(id.clone()),
        sources: vec![id],
    }));
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_cancellation_crosses_threads_without_cancelling_siblings_or_restart() {
        let clock = Arc::new(MasterClock::default());
        let microphone = AudioContext {
            clock: Arc::clone(&clock),
            cancellation: AudioCancellation::default(),
        };
        let application = AudioContext {
            clock,
            cancellation: AudioCancellation::default(),
        };
        assert!(Arc::ptr_eq(&microphone.clock, &application.clock));
        let worker = microphone.cancellation.clone();
        let old_generation = microphone.cancellation.child();
        let next_generation = microphone.cancellation.child();
        old_generation.cancel();
        assert!(!microphone.cancellation.is_cancelled());
        assert!(!next_generation.is_cancelled());
        std::thread::spawn(move || worker.cancel()).join().unwrap();
        assert!(microphone.cancellation.is_cancelled());
        assert!(next_generation.is_cancelled());
        assert!(!application.cancellation.is_cancelled());
        assert!(!AudioCancellation::default().is_cancelled());
    }

    fn route(id: &str, route: AudioRoute) -> AudioRouteSelection {
        AudioRouteSelection {
            controls: Default::default(),
            muted: false,
            source_id: AudioSourceId(id.into()),
            route,
        }
    }
    #[test]
    fn clean_stems_do_not_contain_the_main_mix() {
        let routes = vec![
            route("mic", AudioRoute::MainMixAndSeparate),
            route("app", AudioRoute::SeparateOnly),
            route("ignored", AudioRoute::Ignore),
        ];
        let tracks = plan_tracks(&routes).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].sources, vec![AudioSourceId("mic".into())]);
        assert_eq!(tracks[1].sources, vec![AudioSourceId("app".into())]);
        assert_eq!(tracks[2].sources, vec![AudioSourceId("mic".into())]);
        let mut reverse = routes;
        reverse.reverse();
        assert_eq!(tracks, plan_tracks(&reverse).unwrap());
    }
    #[test]
    fn duplicate_and_ignored_routes() {
        assert!(
            plan_tracks(&[
                route("mic", AudioRoute::Ignore),
                route("mic", AudioRoute::MainMix)
            ])
            .is_err()
        );
        assert!(
            plan_tracks(&[route("mic", AudioRoute::Ignore)])
                .unwrap()
                .is_empty()
        );
    }
}
