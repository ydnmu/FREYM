// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! The accepted `AudioPipeline` output feeding one synchronous Opus encoder per
//! planned track.
//!
//! [`EncodedAudioPipeline`] owns an already started [`AudioPipeline`] and the same
//! recording origin, and builds exactly one [`OpusTrackEncoder`] per frozen
//! [`TrackPlan`] entry, in plan order. It opens no native session, invents no clock,
//! origin or track, and it mixes nothing, shifts no timestamp and resamples nothing.
//!
//! There is no queue, no history and no worker: `poll` encodes at most one mixer
//! quantum and returns what that quantum completed, and `poll_drain` flushes every
//! encoder exactly once *after* the pipeline reported `Drained`. Both batches are
//! bounded by the plan ceiling and ordered by PTS then track id.

use crate::EncodedPacket;
use crate::opus::{OpusTrackEncoder, OpusTrackFinish};
use frame_audio::{AudioPipeline, DrainableAudioSession, PcmQuantum, PipelineDrainPoll, TrackPlan};
use frame_clock::MediaTimestamp;
use frame_clock::PauseTimeline;
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use std::sync::{Arc, Mutex};

/// Conservative ceiling derived from the mixer's PCM byte budget. Actual batches
/// are bounded by the frozen track plan, without a product source-count limit.
pub const MAX_TRACKS: usize = (32 * 1024 * 1024) / (480 * 2 * 4);
/// Live batch ceiling: one 10 ms quantum is half an Opus frame, so a track completes
/// at most one packet per live `poll`.
pub const MAX_LIVE_PACKETS: usize = MAX_TRACKS;
/// Finish batch ceiling: `finish` emits at most two flush packets per track, so one
/// one-shot finish report carries at most twice the frozen track count.
pub const MAX_FINISH_PACKETS: usize = 2 * MAX_TRACKS;

/// The one-shot finalization of a completed drain.
#[derive(Debug)]
pub struct EncodedAudioFinish {
    /// Every final packet of every track, ordered by PTS then track id, at most
    /// [`MAX_FINISH_PACKETS`].
    pub packets: Vec<EncodedPacket>,
    /// The existing [`OpusTrackFinish`] metadata of every track, in plan order. The
    /// `packets` field of each entry is empty: those packets are in `packets` above,
    /// because one batch is ordered by PTS instead of grouped by track. `pre_skip`,
    /// `discard_padding` and `input_frames` are unchanged.
    pub tracks: Vec<OpusTrackFinish>,
}

/// One step of a graceful encoded drain. It mirrors [`PipelineDrainPoll`] and adds the
/// one-shot encoder flush that follows a `Drained` pipeline.
#[derive(Debug)]
pub enum EncodedAudioDrainPoll {
    /// One live drain quantum was encoded: the last short quantum is an ordinary
    /// quantum. The batch holds at most one packet per track (at most
    /// [`MAX_LIVE_PACKETS`]) and is ordered by PTS then track id. It is empty when the
    /// quantum's frames only filled the encoders' pending Opus frames and no packet
    /// completed, so an empty batch is never completion.
    Packets(Vec<EncodedPacket>),
    /// The pipeline drained and every encoder was finished exactly once: the final
    /// packets and the flush metadata are handed over now, once.
    Finished(EncodedAudioFinish),
    /// No packet is ready: a source is still draining, a lock is briefly held, or the
    /// next quantum's range is not covered yet. Never completion and never an error.
    Pending,
    /// The drain completed and its finish report was handed over. Repeated polls stay
    /// `Drained`.
    Drained,
}

/// A stopped or failed pipeline never returns to `Live`, and a stopped flow is never
/// a completed drain.
enum State {
    /// The owned pipeline is still capturing; the live `poll` is the valid input path.
    Live,
    /// A graceful drain owns the tail; only `poll_drain` may advance it.
    Draining,
    /// The pipeline drained and every encoder was finished exactly once.
    Finished,
    /// `Some` is the stored first failure; `None` is an explicit stop or abort.
    Terminal(Option<RecorderError>),
}

/// Connects one `AudioPipeline` to one `OpusTrackEncoder` per planned track.
///
/// The constructor takes the pipeline's ownership and the same media origin, so no
/// second session list, clock or epoch exists. This type owns no worker, timer, sleep
/// or output queue: the caller chooses when to poll and owns every returned packet.
pub struct EncodedAudioPipeline<S: DrainableAudioSession> {
    pipeline: AudioPipeline<S>,
    /// One encoder per planned track, in plan order: the index is the plan index, the
    /// `TrackPcm::track_index` and the packet `track_id`.
    encoders: Vec<OpusTrackEncoder>,
    settings: frame_contracts::AudioSettings,
    origin: MediaTimestamp,
    /// Absolute start of the current codec instance; the source/mixer origin stays fixed.
    codec_origin: MediaTimestamp,
    state: State,
}

impl<S: DrainableAudioSession> EncodedAudioPipeline<S> {
    pub fn set_pause_timeline(&mut self, timeline: Arc<Mutex<PauseTimeline>>) {
        self.pipeline.set_pause_timeline(timeline);
    }
    /// Takes ownership of an already started pipeline and the **same** media origin it
    /// was started with, and builds one encoder per frozen plan entry.
    ///
    /// An `Ignore` route is never in the plan and an empty plan builds no encoder, so
    /// no empty or ignored track ever gets an encoder. When one encoder cannot be
    /// created, every session is closed through the owned pipeline, the original error
    /// code is kept and any cleanup failure is reported beside it.
    pub fn new(pipeline: AudioPipeline<S>, origin: MediaTimestamp) -> Result<Self, RecorderError> {
        Self::with_settings(pipeline, origin, frame_contracts::AudioSettings::default())
    }

    pub fn with_settings(
        mut pipeline: AudioPipeline<S>,
        origin: MediaTimestamp,
        settings: frame_contracts::AudioSettings,
    ) -> Result<Self, RecorderError> {
        if let Err(error) = settings.validate() {
            return Err(with_cleanup(error, pipeline.stop()));
        }
        let track_count = pipeline.tracks().len();
        let mut encoders = Vec::with_capacity(track_count);
        for track_index in 0..track_count {
            match OpusTrackEncoder::with_settings(
                track_index,
                origin,
                settings.track_channels(pipeline.track_channels(track_index)),
                &settings,
            ) {
                Ok(encoder) => encoders.push(encoder),
                // The pipeline owns every session, so closing it is the cleanup.
                Err(error) => return Err(with_cleanup(error, pipeline.stop())),
            }
        }
        debug_assert!(
            encoders.len() <= MAX_TRACKS,
            "the frozen plan cannot exceed the mixer's route ceiling"
        );
        Ok(Self {
            pipeline,
            encoders,
            settings,
            origin,
            codec_origin: origin,
            state: State::Live,
        })
    }

    /// The frozen track plan, in canonical main-first/clean-ID order: the order of
    /// [`Self::pre_skip`] and of the finish report. Plan index `i` is also the packet
    /// `track_id`.
    pub fn set_muted(
        &mut self,
        id: &frame_contracts::AudioSourceId,
        muted: bool,
    ) -> Result<(), RecorderError> {
        self.pipeline.set_muted(id, muted)
    }
    pub fn track_channels(&self, index: usize) -> std::num::NonZeroU16 {
        self.settings
            .track_channels(self.pipeline.track_channels(index))
    }
    pub fn settings(&self) -> frame_contracts::AudioSettings {
        self.settings
    }
    pub fn tracks(&self) -> &[TrackPlan] {
        self.pipeline.tracks()
    }

    pub fn clock_health(&self) -> impl Iterator<Item = frame_audio::SourceClockHealth> + '_ {
        self.pipeline.clock_health()
    }

    pub fn source_lifecycle(
        &self,
    ) -> impl Iterator<Item = frame_audio::SourceLifecycleSnapshot> + '_ {
        self.pipeline.source_lifecycle()
    }

    pub fn replace_source_device<B: frame_audio::AudioCaptureBackend<Session = S>>(
        &mut self,
        logical: &frame_contracts::AudioSourceId,
        physical: &frame_contracts::AudioSourceId,
        backend: &mut B,
    ) -> Result<(), RecorderError> {
        if !matches!(self.state, State::Live) {
            return Err(drain_owns_the_tail());
        }
        self.pipeline
            .replace_source_device(logical, physical, backend)
    }

    /// The shared media origin the encoders were built with, for a container header.
    pub fn origin(&self) -> MediaTimestamp {
        self.origin
    }

    /// Ends the current codec instances at the next mixer quantum boundary, then
    /// opens fresh Opus instances without stopping or rebasing the source pipeline.
    /// Returned packets and trims belong only to the preceding container segment.
    /// The next live quantum keeps its original absolute timestamp.
    pub fn rotate_segment(&mut self) -> Result<EncodedAudioFinish, RecorderError> {
        match &self.state {
            State::Live => {}
            State::Terminal(Some(error)) => return Err(error.clone()),
            _ => return Err(drain_owns_the_tail()),
        }
        let finish = self.finish_encoders()?;
        let frames = finish.tracks.first().map_or(0, |track| track.input_frames);
        if finish
            .tracks
            .iter()
            .any(|track| track.input_frames != frames)
        {
            return Err(self.fail_codec(internal(
                "Audio segment tracks ended at different sample boundaries.".into(),
            )));
        }
        // A live mixer emits whole 480-frame quanta. This exact boundary preserves
        // sample-derived timestamps; no clock observation is used as an audio cut.
        if !frames.is_multiple_of(480) {
            return Err(self.fail_codec(internal(
                "A live audio segment did not end at a mixer quantum boundary.".into(),
            )));
        }
        let boundary = u128::from(frames) * 1_000_000_000 / 48_000;
        let Some(origin) = u64::try_from(boundary)
            .ok()
            .and_then(|offset| self.codec_origin.0.checked_add(offset))
        else {
            return Err(self.fail_codec(internal("Audio segment timestamp overflowed.".into())));
        };
        let mut encoders = Vec::with_capacity(self.encoders.len());
        for index in 0..self.encoders.len() {
            match OpusTrackEncoder::with_settings(
                index,
                MediaTimestamp(origin),
                self.track_channels(index),
                &self.settings,
            ) {
                Ok(encoder) => encoders.push(encoder),
                Err(error) => return Err(self.fail_codec(error)),
            }
        }
        self.encoders = encoders;
        self.codec_origin = MediaTimestamp(origin);
        Ok(finish)
    }

    pub fn segment_origin(&self) -> MediaTimestamp {
        self.codec_origin
    }

    /// The encoder's real Opus lookahead for one plan index, readable before any PCM
    /// arrives so a container can write the OpusHead pre-skip early. `None` is an
    /// index outside the plan.
    pub fn pre_skip(&self, track_index: usize) -> Option<u16> {
        self.encoders
            .get(track_index)
            .map(OpusTrackEncoder::pre_skip)
    }

    /// The stored failure once the flow became terminal through an error. A clean stop
    /// or an aborted drain is not a failure.
    pub fn failure(&self) -> Option<&RecorderError> {
        match &self.state {
            State::Terminal(Some(error)) => Some(error),
            _ => None,
        }
    }

    /// Reads at most one quantum from the owned pipeline and encodes every track of it
    /// through that track's own encoder.
    ///
    /// The returned batch is bounded by [`MAX_LIVE_PACKETS`] and ordered by PTS then
    /// track id. An empty batch means the pipeline had no quantum ready or the quantum
    /// only filled the encoders' pending Opus frames; both mean "poll again" and
    /// neither is completion. No mixing, no timestamp shift and no resampling happens
    /// here, and no `Ignore` route is ever opened.
    ///
    /// A pipeline failure or an encoder rejection is terminal: the original code is
    /// kept, the owned pipeline is closed and every later call repeats the failure.
    /// While a drain owns the tail this live path is invalid and never disturbs the
    /// drain. Failure cleanup can join native workers: call from an owner/control
    /// thread, never a UI thread or realtime callback.
    pub fn poll(&mut self, now: MediaTimestamp) -> Result<Vec<EncodedPacket>, RecorderError> {
        match &self.state {
            State::Live => {}
            State::Draining => return Err(drain_owns_the_tail()),
            State::Finished => return Err(drain_completed()),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) => return Err(stopped()),
        }
        let quantum = match self.pipeline.poll(now) {
            Ok(quantum) => quantum,
            // The pipeline already closed every session when it stored this failure.
            Err(error) => return Err(self.fail_pipeline(error)),
        };
        match quantum {
            Some(quantum) => self.encode(quantum),
            None => Ok(Vec::new()),
        }
    }

    /// Reads at most one block from each owned source into the aligned processor and
    /// returns how many were accepted, without waiting for a mixer quantum.
    ///
    /// A live owner that paces the mixer slower than its sources produce still has to
    /// take the native PCM off every source's bounded queue. This forwards exactly the
    /// accepted source-reading path: no mixing, no encoding, no timestamp shift and no
    /// `Ignore` route is opened. A pipeline failure is terminal and keeps its code; an
    /// encoder failure is impossible here because no encoder runs.
    pub fn poll_sources(&mut self) -> Result<usize, RecorderError> {
        match &self.state {
            State::Live => {}
            State::Draining => return Err(drain_owns_the_tail()),
            State::Finished => return Err(drain_completed()),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) => return Err(stopped()),
        }
        match self.pipeline.poll_sources() {
            Ok(accepted) => Ok(accepted),
            Err(error) => Err(self.fail_pipeline(error)),
        }
    }

    /// Idempotent. Forwards the request to the owned pipeline, which never cancels,
    /// stops, joins or discards audio: a repeated request notifies no source twice. A
    /// completed drain is not reopened and a terminal flow repeats its stored failure.
    pub fn request_drain(&mut self) -> Result<(), RecorderError> {
        match &self.state {
            // Forwarded, not short-circuited: the pipeline owns idempotency.
            State::Live | State::Draining => {}
            State::Finished => return Ok(()),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) => return Err(stopped()),
        }
        match self.pipeline.request_drain() {
            Ok(()) => {
                self.state = State::Draining;
                Ok(())
            }
            Err(error) => Err(self.fail_pipeline(error)),
        }
    }

    /// One graceful tail step, mirroring [`PipelineDrainPoll`], plus the one-shot
    /// encoder flush.
    ///
    /// A live quantum is encoded exactly like the live path, so the final shorter
    /// quantum is encoded too. `Pending` is repeated unchanged and never finishes an
    /// encoder. Only once the pipeline reports `Drained` are the encoders finished,
    /// once each in plan order; the final packets and the existing flush metadata are
    /// handed over once, and every later poll returns `Drained`.
    ///
    /// An encoder or pipeline failure is terminal, keeps the original code, preserves
    /// a cleanup failure beside it and closes the owned pipeline. An aborted drain
    /// never reports `Drained`.
    pub fn poll_drain(&mut self) -> Result<EncodedAudioDrainPoll, RecorderError> {
        match &self.state {
            State::Draining => {}
            State::Live => return Err(drain_was_not_requested()),
            State::Finished => return Ok(EncodedAudioDrainPoll::Drained),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) => return Err(stopped()),
        }
        match self.pipeline.poll_drain() {
            // The last, shorter piece of the tail is an ordinary quantum here.
            Ok(PipelineDrainPoll::Quantum(quantum)) => {
                self.encode(quantum).map(EncodedAudioDrainPoll::Packets)
            }
            Ok(PipelineDrainPoll::Pending) => Ok(EncodedAudioDrainPoll::Pending),
            Ok(PipelineDrainPoll::Drained) => {
                // Only now: the mixer emitted every accepted frame and every session is
                // closed, so no further PCM can arrive at an encoder.
                let finish = self.finish_encoders()?;
                self.state = State::Finished;
                Ok(EncodedAudioDrainPoll::Finished(finish))
            }
            Err(error) => Err(self.fail_pipeline(error)),
        }
    }

    /// Aborts without ever reporting a completed drain.
    ///
    /// A running or draining flow cancels, stops and drops every session through the
    /// owned pipeline, and every later poll is an explicit `InvalidTransition` instead
    /// of `Drained`; the encoders are abandoned, never flushed. A failed flow repeats
    /// its stored error, and a fully completed drain stays complete.
    pub fn stop(&mut self) -> Result<(), RecorderError> {
        match &self.state {
            State::Terminal(Some(error)) => return Err(error.clone()),
            // A clean stop and a fully completed drain are both already clean: a
            // successful drain is not turned into an abort by a later stop.
            State::Terminal(None) | State::Finished => return Ok(()),
            State::Live | State::Draining => {}
        }
        match self.pipeline.stop() {
            Ok(()) => {
                self.state = State::Terminal(None);
                Ok(())
            }
            Err(error) => {
                self.state = State::Terminal(Some(error.clone()));
                Err(error)
            }
        }
    }

    /// Encodes one quantum track by track, through the encoder that owns that track,
    /// and returns the completed packets in PTS/track-id order.
    fn encode(&mut self, quantum: PcmQuantum) -> Result<Vec<EncodedPacket>, RecorderError> {
        let mut packets = Vec::with_capacity(self.encoders.len());
        for mut track in quantum.tracks {
            // Map channels after source processing, before each independent
            // encoder. Frame counts and the sample timeline remain unchanged.
            let output_channels = self.settings.track_channels(track.channels);
            if track.channels != output_channels {
                if output_channels.get() == 1 {
                    let frames = track.samples.len() / 2;
                    for frame in 0..frames {
                        track.samples[frame] =
                            track.samples[frame * 2] * 0.5 + track.samples[frame * 2 + 1] * 0.5;
                    }
                    track.samples.truncate(frames);
                } else {
                    let frames = track.samples.len();
                    track.samples.resize(frames * 2, 0.0);
                    for frame in (0..frames).rev() {
                        let sample = track.samples[frame];
                        track.samples[frame * 2] = sample;
                        track.samples[frame * 2 + 1] = sample;
                    }
                }
                track.channels = output_channels;
            }
            let track_index = track.track_index;
            let outcome = match self.encoders.get_mut(track_index) {
                Some(encoder) => {
                    if usize::try_from(encoder.track_id()).ok() == Some(track_index) {
                        encoder.submit(&track)
                    } else {
                        Err(internal(format!(
                            "The Opus encoder of track {track_index} carries a different identity."
                        )))
                    }
                }
                None => Err(internal(format!(
                    "The mixer returned track {track_index} without a planned Opus encoder."
                ))),
            };
            match outcome {
                Ok(Some(packet)) => packets.push(packet),
                Ok(None) => {}
                // Terminal: the frames of this quantum are not silently re-timed or
                // dropped into a later batch.
                Err(error) => return Err(self.fail_codec(error)),
            }
        }
        debug_assert!(
            packets.len() <= self.encoders.len(),
            "one half-frame quantum completes at most one packet per track"
        );
        sort_packets(&mut packets);
        Ok(packets)
    }

    /// Finishes every encoder exactly once, in plan order, and returns the one-shot
    /// report: the final packets of every track ordered by PTS then track id, plus the
    /// existing flush metadata in plan order.
    ///
    /// A failure stops the flush at that encoder and is terminal; the packets of the
    /// encoders after it are abandoned, never reported as a completed drain.
    fn finish_encoders(&mut self) -> Result<EncodedAudioFinish, RecorderError> {
        let mut packets = Vec::new();
        let mut tracks = Vec::with_capacity(self.encoders.len());
        let mut failure: Option<RecorderError> = None;
        for encoder in &mut self.encoders {
            match encoder.finish() {
                Ok(finish) => {
                    let OpusTrackFinish {
                        packets: final_packets,
                        pre_skip,
                        discard_padding,
                        input_frames,
                    } = finish;
                    packets.extend(final_packets);
                    // The metadata stays in plan order; the packets join the one batch
                    // that is ordered by PTS instead of grouped by track.
                    tracks.push(OpusTrackFinish {
                        packets: Vec::new(),
                        pre_skip,
                        discard_padding,
                        input_frames,
                    });
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failure {
            return Err(self.fail_codec(error));
        }
        debug_assert!(
            packets.len() <= self.encoders.len() * 2,
            "a flush emits at most two packets per track"
        );
        sort_packets(&mut packets);
        Ok(EncodedAudioFinish { packets, tracks })
    }

    /// Stores a failure the owned pipeline already cleaned up after.
    fn fail_pipeline(&mut self, error: RecorderError) -> RecorderError {
        self.state = State::Terminal(Some(error.clone()));
        error
    }

    /// Stores an encoder failure after closing the owned pipeline, keeping the
    /// original code and reporting a failing cleanup beside it exactly once.
    fn fail_codec(&mut self, error: RecorderError) -> RecorderError {
        let error = with_cleanup(error, self.pipeline.stop());
        self.state = State::Terminal(Some(error.clone()));
        error
    }
}

impl<S: DrainableAudioSession> Drop for EncodedAudioPipeline<S> {
    fn drop(&mut self) {
        // Session `Drop` is the documented second cleanup attempt.
        let _ = self.stop();
    }
}

/// Batch order: PTS first, then the stable plan index (`track_id`). One quantum yields
/// one PTS, and interleaved flush packets stay on one media timeline.
fn sort_packets(packets: &mut [EncodedPacket]) {
    packets.sort_by_key(|packet| (packet.pts.0, packet.track_id));
}

/// Keeps the primary failure and its code while preserving the cleanup failure.
fn with_cleanup(mut error: RecorderError, cleanup: Result<(), RecorderError>) -> RecorderError {
    if let Err(cleanup) = cleanup {
        error.message = format!("{} Cleanup also failed: {}", error.message, cleanup.message);
    }
    error
}

fn stopped() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "The encoded audio pipeline has been stopped.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new audio pipeline for the next recording.".into(),
    }
}

fn internal(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "The audio group is failed; start a new recording.".into(),
    }
}

fn drain_was_not_requested() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "The encoded audio pipeline drain was not requested.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Call request_drain before poll_drain.".into(),
    }
}

fn drain_owns_the_tail() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "The encoded audio pipeline is draining; the live poll is unavailable.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Finish the drain with poll_drain, or abort it with stop.".into(),
    }
}

fn drain_completed() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "The encoded audio pipeline completed its drain; the live poll is unavailable."
            .into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new audio pipeline for the next recording.".into(),
    }
}
