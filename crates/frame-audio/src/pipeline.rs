// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Live capture sessions feeding the accepted aligned PCM processor (ADR 0009).

use crate::metering::MeterHub;
use crate::{
    AlignedPcmProcessor, AudioCancellation, AudioCaptureBackend, AudioContext, AudioSession,
    DrainableAudioSession, PcmBlock, PcmDrainPoll, PcmQuantum, TrackPlan,
};
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline, sample_timestamp};
use frame_contracts::{
    AudioRoute, AudioRouteSelection, AudioSourceId, ErrorCode, RecorderError, RecoveryStatus,
};
use std::sync::{Arc, Mutex};

/// One selected, non-ignored route with its own cancellation token and live session.
struct Source<S> {
    id: AudioSourceId,
    capture_id: AudioSourceId,
    replacements: u64,
    generation: u64,
    continuity_pending: bool,
    continuity_clipped_frames: u64,
    cancellation: AudioCancellation,
    session: Option<S>,
    clock_health: Option<frame_clock::DriftEstimate>,
    accepted_raw_end: Option<MediaTimestamp>,
    replacement_after: Option<MediaTimestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLifecycleSnapshot {
    /// Frozen routing/track identity; unaffected by a physical-device replacement.
    pub source_id: AudioSourceId,
    pub capture_source_id: AudioSourceId,
    pub replacements: u64,
    /// Normalized frames already covered by the old source or a published
    /// silence quantum, discarded only at an explicit continuity transition.
    pub continuity_clipped_frames: u64,
    pub state: crate::SourceLifecycle,
    pub compensation: Option<crate::DriftCompensationHealth>,
}

/// A stopped or failed group never returns to `Running`.
enum State {
    Running,
    /// A graceful drain owns the tail; the live `poll` refuses until it completes.
    Draining,
    /// `Some` is the stored first failure; `None` is an explicit clean stop or a
    /// completed drain.
    Terminal(Option<RecorderError>),
}

/// One step of a graceful `AudioPipeline` drain. It mirrors the accepted source
/// verdicts, but a quantum is the pipeline's own mixer output.
#[derive(Debug, PartialEq)]
pub enum PipelineDrainPoll {
    /// At most one quantum, identical to what the live `poll` would have returned.
    Quantum(PcmQuantum),
    /// No quantum is ready: a source is still draining, a lock is briefly held, or
    /// the next quantum's range is not yet covered by every open source. `Pending`
    /// is never a verdict, never proves a completed drain and is never an error.
    Pending,
    /// The mixer finished and every session is closed and dropped. Repeated polls
    /// stay `Drained`.
    Drained,
}

/// Connects the selected `AudioSession`s of one backend to one `AlignedPcmProcessor`.
///
/// The owner supplies the recording's single `Arc<MasterClock>` and media origin, so no
/// source invents its own epoch. This type owns no worker, timer, sleep or output queue:
/// the caller chooses when to poll and owns every returned quantum. It never manages
/// video, other tracks or the UI.
pub struct AudioPipeline<S: AudioSession> {
    clock: Arc<MasterClock>,
    pause_timeline: Option<Arc<Mutex<PauseTimeline>>>,
    processor: AlignedPcmProcessor,
    sources: Vec<Source<S>>,
    state: State,
    /// One flag per source: its `Drained` verdict was already handed to the mixer
    /// through exactly one `end_source`.
    reported_drained: Vec<bool>,
    /// The mixer's one-shot `finish` has run.
    mixer_finished: bool,
    /// The drain completed: the mixer is drained and every session is closed.
    drain_complete: bool,
    meter: Option<(Arc<MeterHub>, String)>,
}

impl<S: AudioSession> AudioPipeline<S> {
    /// Opens every selected route through `backend` in stable source-ID order.
    ///
    /// `Ignore` routes are frozen into the plan but never opened. Each opened source gets
    /// its own fresh cancellation token; all of them share the one supplied clock. When an
    /// open fails, every session opened before it is cancelled, stopped and dropped before
    /// the primary error (with any cleanup failure appended) is returned.
    pub fn start<B>(
        backend: &mut B,
        routes: &[AudioRouteSelection],
        clock: Arc<MasterClock>,
        origin: MediaTimestamp,
    ) -> Result<Self, RecorderError>
    where
        B: AudioCaptureBackend<Session = S>,
    {
        // Validate the source plan and its memory budget before opening sessions.
        let mut processor = AlignedPcmProcessor::new(routes, origin)?;
        backend.validate_capture_routes(routes)?;
        let mut selected: Vec<AudioSourceId> = routes
            .iter()
            .filter(|route| route.route != AudioRoute::Ignore)
            .map(|route| route.source_id.clone())
            .collect();
        selected.sort_unstable();
        let mut sources = Vec::with_capacity(selected.len());
        for id in selected {
            let cancellation = AudioCancellation::default();
            let context = AudioContext {
                clock: Arc::clone(&clock),
                cancellation: cancellation.clone(),
            };
            match backend.start_audio(&id, context) {
                Ok(mut session) => {
                    if let Err(error) = processor.set_source_channels(&id, session.pcm_channels()) {
                        let cleanup = session.stop();
                        return Err(with_cleanup(
                            with_cleanup(error, cleanup),
                            stop_sources(&mut sources),
                        ));
                    }
                    let generation = session.lifecycle().generation();
                    sources.push(Source {
                        capture_id: id.clone(),
                        replacements: 0,
                        generation,
                        continuity_pending: false,
                        continuity_clipped_frames: 0,
                        id,
                        cancellation,
                        session: Some(session),
                        clock_health: None,
                        accepted_raw_end: None,
                        replacement_after: None,
                    });
                }
                Err(error) => return Err(with_cleanup(error, stop_sources(&mut sources))),
            }
        }
        Ok(Self {
            clock,
            pause_timeline: None,
            processor,
            reported_drained: vec![false; sources.len()],
            sources,
            state: State::Running,
            mixer_finished: false,
            drain_complete: false,
            meter: None,
        })
    }

    /// Observes source PCM before it moves into the existing mixer. The observer owns
    /// only numeric latest values, never a block or a source session.
    pub fn attach_meter(&mut self, meter: Arc<MeterHub>, session_id: String) {
        self.meter = Some((meter, session_id));
    }

    /// The frozen track plan, in canonical main-first/clean-ID order.
    pub fn set_muted(&mut self, id: &AudioSourceId, muted: bool) -> Result<(), RecorderError> {
        self.processor.set_muted(id, muted)
    }
    pub fn track_channels(&self, index: usize) -> std::num::NonZeroU16 {
        self.processor.track_channels(index)
    }
    pub fn tracks(&self) -> &[TrackPlan] {
        self.processor.tracks()
    }

    /// Latest measured raw source-clock health, retained after source teardown.
    pub fn clock_health(&self) -> impl Iterator<Item = crate::SourceClockHealth> + '_ {
        self.sources.iter().filter_map(|source| {
            source.clock_health.map(|drift| crate::SourceClockHealth {
                source_id: source.id.clone(),
                drift,
            })
        })
    }

    /// The plan is frozen for this pipeline: a source outage is a measurable gap,
    /// never track deletion or implicit replacement by the OS default device.
    pub fn source_lifecycle(&self) -> impl Iterator<Item = SourceLifecycleSnapshot> + '_ {
        self.sources.iter().map(|source| SourceLifecycleSnapshot {
            source_id: source.id.clone(),
            capture_source_id: source.capture_id.clone(),
            replacements: source.replacements,
            continuity_clipped_frames: source.continuity_clipped_frames,
            state: source.session.as_ref().map_or(
                crate::SourceLifecycle::Ended {
                    generation: source.generation,
                },
                AudioSession::lifecycle,
            ),
            compensation: source
                .session
                .as_ref()
                .and_then(AudioSession::compensation_health),
        })
    }

    /// Explicit device hot-swap on the owner thread. Open/validate readiness of
    /// the requested physical ID before closing the old session. Frozen track
    /// order and the shared clock stay unchanged; input timestamp mapping stays
    /// native, and only the physical PCM tag is mapped to the existing route ID.
    /// New/removed routes require a new pipeline and container segment.
    pub fn replace_source_device<B>(
        &mut self,
        logical_source: &AudioSourceId,
        physical_source: &AudioSourceId,
        backend: &mut B,
    ) -> Result<(), RecorderError>
    where
        B: AudioCaptureBackend<Session = S>,
    {
        if !matches!(self.state, State::Running) {
            return Err(RecorderError::invalid(
                "Device replacement requires a running audio pipeline.",
            ));
        }
        let index = self
            .sources
            .iter()
            .position(|source| &source.id == logical_source)
            .ok_or_else(|| {
                RecorderError::invalid("The replacement route is not in the frozen track plan.")
            })?;
        if self
            .sources
            .iter()
            .enumerate()
            .any(|(other, source)| other != index && &source.capture_id == physical_source)
        {
            return Err(RecorderError::invalid(
                "The physical source is already owned by another frozen route.",
            ));
        }
        let replacement_routes: Vec<_> = self
            .sources
            .iter()
            .enumerate()
            .map(|(other, source)| AudioRouteSelection {
                source_id: if other == index {
                    physical_source.clone()
                } else {
                    source.capture_id.clone()
                },
                route: AudioRoute::MainMix,
                muted: false,
                controls: Default::default(),
            })
            .collect();
        backend.validate_capture_routes(&replacement_routes)?;
        let cancellation = AudioCancellation::default();
        let mut replacement = backend.start_audio(
            physical_source,
            AudioContext {
                clock: Arc::clone(&self.clock),
                cancellation: cancellation.clone(),
            },
        )?;
        if replacement.pcm_channels()
            != self.sources[index]
                .session
                .as_ref()
                .map_or(replacement.pcm_channels(), AudioSession::pcm_channels)
        {
            let _ = replacement.stop();
            return Err(RecorderError::invalid(
                "A device replacement must preserve the frozen channel layout.",
            ));
        }
        self.sources[index].cancellation.cancel();
        if let Some(old) = self.sources[index].session.as_mut()
            && let Err(error) = old.stop_for_continuity()
        {
            let error = with_cleanup(error, replacement.stop());
            return Err(self.fail(error));
        }
        let source = &mut self.sources[index];
        source.session = Some(replacement);
        source.cancellation = cancellation;
        source.capture_id = physical_source.clone();
        source.replacements = source.replacements.saturating_add(1);
        source.generation = source
            .session
            .as_ref()
            .expect("replacement is installed")
            .lifecycle()
            .generation();
        source.continuity_pending = true;
        source.clock_health = None;
        source.replacement_after = source.accepted_raw_end;
        Ok(())
    }

    pub fn set_pause_timeline(&mut self, timeline: Arc<Mutex<PauseTimeline>>) {
        self.pause_timeline = Some(timeline);
    }

    /// The stored failure when the group became terminal through an error.
    pub fn failure(&self) -> Option<&RecorderError> {
        match &self.state {
            State::Terminal(Some(error)) => Some(error),
            _ => None,
        }
    }

    /// Reads at most one block from each source and returns at most one quantum.
    ///
    /// A source block tagged with a different identity, an unsupported rate or layout, a
    /// source error and a mixer rejection are all terminal: every session is closed and the
    /// error is stored, so a later poll repeats it instead of losing or hiding it.
    /// While a graceful drain is running this live path is invalid instead; the drain
    /// owns the tail and is never abandoned by a rejected poll.
    /// Failure cleanup can join native workers: call from an owner/control thread,
    /// never a UI thread or realtime callback.
    pub fn poll(&mut self, now: MediaTimestamp) -> Result<Option<PcmQuantum>, RecorderError> {
        self.admit_ready_blocks()?;
        match self.processor.poll(now) {
            Ok(quantum) => Ok(quantum),
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Reads at most one block from each source into the aligned processor and returns how
    /// many were accepted, without waiting for the mixer to produce a quantum.
    ///
    /// A live owner that paces the mixer slower than the sources produce (for example
    /// because the display side is static at that moment) still has to take the native
    /// PCM off each source's bounded queue. This is that entry point: it reads no more
    /// than the accepted live path and changes nothing about the mixer's own rules, so
    /// no block is dropped, re-timed or split.
    pub fn poll_sources(&mut self) -> Result<usize, RecorderError> {
        self.admit_ready_blocks()
    }

    /// One block from each source, under the frozen ADR 0009 policy. Shared by the live poll
    /// and by the source-only poll, so both apply exactly the same rules — including the
    /// state guard, so a terminal group always repeats its stored error and never touches a
    /// session it already released.
    fn admit_ready_blocks(&mut self) -> Result<usize, RecorderError> {
        match &self.state {
            State::Running => {}
            State::Draining => return Err(drain_owns_the_tail()),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) => return Err(stopped()),
        }
        let mut accepted = 0;
        for index in 0..self.sources.len() {
            let expected = self.sources[index].id.clone();
            // A running source always owns its session; losing it is an explicit failure,
            // never a silent skip that would look like an idle source.
            let read = match self.sources[index].session.as_mut() {
                Some(session) => session.try_pcm(),
                None => Err(internal(format!(
                    "Audio source {} lost its session while still running.",
                    expected.0
                ))),
            };
            match read {
                Ok(None) => {}
                Ok(Some(block)) => {
                    self.accept_block(index, block)?;
                    accepted += 1;
                }
                Err(error) => return Err(self.fail(error)),
            }
        }
        Ok(accepted)
    }

    /// Idempotent. Cancels every source, attempts every session stop, then drops every
    /// session even when an earlier stop failed. A terminal group is left untouched;
    /// its stored error is returned again, including on repeated stop calls.
    ///
    /// Stopping during a drain is the existing abort: the request is not honoured,
    /// remaining capture is cancelled and no `Drained` verdict is ever reported.
    pub fn stop(&mut self) -> Result<(), RecorderError> {
        match &self.state {
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) => return Ok(()),
            State::Running | State::Draining => {}
        }
        match stop_sources(&mut self.sources) {
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

    /// One block just read from a known source, under the frozen ADR 0009 policy.
    ///
    /// The identity check and the mixer's rate, layout, length, timeline and capacity
    /// rules decide; a rejection closes every session and stores the failure with its
    /// code, so a later poll repeats it instead of losing or hiding it.
    fn accept_block(&mut self, index: usize, mut block: PcmBlock) -> Result<(), RecorderError> {
        let generation = self.sources[index]
            .session
            .as_ref()
            .expect("an admitted source owns its session")
            .lifecycle()
            .generation();
        if generation != self.sources[index].generation {
            let source = &mut self.sources[index];
            source.generation = generation;
            source.continuity_pending = true;
            source.replacement_after = source.accepted_raw_end;
        }
        self.sources[index].clock_health = self.sources[index]
            .session
            .as_ref()
            .and_then(AudioSession::clock_drift);
        let expected = self.sources[index].id.clone();
        let physical = &self.sources[index].capture_id;
        if &block.source_id != physical {
            return Err(self.fail(RecorderError::invalid(format!(
                "Audio source {} returned PCM tagged as {}.",
                physical.0, block.source_id.0
            ))));
        }
        block.source_id = expected.clone();
        if self.sources[index].continuity_pending
            || self.sources[index]
                .session
                .as_ref()
                .is_some_and(AudioSession::allows_silent_gaps)
        {
            // A stale block still has to satisfy the PCM contract. Transition
            // trimming must not hide a malformed format or non-finite sample.
            if block.sample_rate.get() != 48_000 || !matches!(block.channels.get(), 1 | 2) {
                return Err(self.fail(RecorderError::unsupported("PCM rate or channel layout")));
            }
            let channels = usize::from(block.channels.get());
            if block.samples.is_empty()
                || !block.samples.len().is_multiple_of(channels)
                || block.samples.len() / channels > 4_800
                || block.samples.iter().any(|sample| !sample.is_finite())
            {
                return Err(self.fail(RecorderError::invalid(
                    "PCM block has invalid length or non-finite samples.",
                )));
            }
        }
        // Discard only overlapping samples during an explicit device switch or
        // a same-source native generation change.
        // Advance by real source samples; preserve every later native timestamp.
        if let Some(boundary) = self.sources[index].replacement_after {
            let skipped = clip_before(&mut block, boundary)?;
            self.sources[index].continuity_clipped_frames = self.sources[index]
                .continuity_clipped_frames
                .saturating_add(skipped);
            if block.samples.is_empty() {
                return Ok(());
            }
            self.sources[index].replacement_after = None;
        }
        let mut accepted_raw_end = sample_timestamp(
            (block.samples.len() / usize::from(block.channels.get())) as u64,
            block.sample_rate,
        )
        .and_then(|duration| block.timestamp.0.checked_add(duration.0))
        .map(MediaTimestamp);
        if let Some((meter, session_id)) = &self.meter {
            meter.observe(session_id, &expected, &block.samples);
        }
        if let Some(timeline) = &self.pause_timeline {
            let timeline = timeline.lock().unwrap_or_else(|poison| poison.into_inner());
            if let Some(boundary) = timeline.resume_after()
                && block.timestamp.0 < boundary.0
            {
                let elapsed = boundary.0 - block.timestamp.0;
                let frames = (u128::from(elapsed) * u128::from(block.sample_rate.get()))
                    .div_ceil(1_000_000_000);
                let channels = usize::from(block.channels.get());
                let skip = usize::try_from(frames).unwrap_or(usize::MAX);
                if skip >= block.samples.len() / channels {
                    return Ok(());
                }
                let offset = sample_timestamp(skip as u64, block.sample_rate)
                    .ok_or_else(|| RecorderError::invalid("PCM resume offset overflowed."))?;
                block.timestamp.0 =
                    block.timestamp.0.checked_add(offset.0).ok_or_else(|| {
                        RecorderError::invalid("PCM resume timestamp overflowed.")
                    })?;
                block.samples.drain(..skip * channels);
            }
            if let Some(boundary) = timeline.pause_boundary_raw() {
                if block.timestamp.0 >= boundary.0 {
                    return Ok(());
                }
                let available = boundary.0 - block.timestamp.0;
                let frames = (u128::from(available) * u128::from(block.sample_rate.get()))
                    .div_ceil(1_000_000_000);
                let keep = usize::try_from(frames)
                    .unwrap_or(usize::MAX)
                    .min(block.samples.len() / usize::from(block.channels.get()));
                if keep == 0 {
                    return Ok(());
                }
                block
                    .samples
                    .truncate(keep * usize::from(block.channels.get()));
            }
            accepted_raw_end = sample_timestamp(
                (block.samples.len() / usize::from(block.channels.get())) as u64,
                block.sample_rate,
            )
            .and_then(|duration| block.timestamp.0.checked_add(duration.0))
            .map(MediaTimestamp);
            block.timestamp = timeline
                .map(block.timestamp)
                .ok_or_else(|| RecorderError::invalid("PCM pause mapping underflowed."))?;
        }
        if self.sources[index].continuity_pending {
            // During an outage the mixer may already have published explicit
            // gap quanta. Fresh native PCM cannot replace that committed media.
            // Clip on the mapped timeline so pause removal remains exact.
            let boundary = self.processor.next_timestamp()?;
            let skipped = clip_before(&mut block, boundary)?;
            self.sources[index].continuity_clipped_frames = self.sources[index]
                .continuity_clipped_frames
                .saturating_add(skipped);
            if block.samples.is_empty() {
                return Ok(());
            }
        }
        // ADR 0009's mixer owns rate/layout policy: it accepts only 48 kHz
        // mono/stereo and returns the rejected block to the caller.
        if let Err((error, _rejected)) = self.processor.push(block) {
            return Err(self.fail(error));
        }
        self.sources[index].accepted_raw_end = accepted_raw_end;
        self.sources[index].continuity_pending = false;
        Ok(())
    }

    /// Closes every session and stores the first failure of the audio group.
    fn fail(&mut self, error: RecorderError) -> RecorderError {
        let error = with_cleanup(error, stop_sources(&mut self.sources));
        self.state = State::Terminal(Some(error.clone()));
        error
    }
}

/// Trim by real samples, preserving the surviving sample's native position.
/// The caller decides when a continuity transition permits dropping overlap.
fn clip_before(block: &mut PcmBlock, boundary: MediaTimestamp) -> Result<u64, RecorderError> {
    if block.timestamp >= boundary {
        return Ok(0);
    }
    let channels = usize::from(block.channels.get());
    let frames = block.samples.len() / channels;
    let requested = (u128::from(boundary.0 - block.timestamp.0)
        * u128::from(block.sample_rate.get()))
    .div_ceil(1_000_000_000);
    let skip = usize::try_from(requested).unwrap_or(usize::MAX).min(frames);
    block.samples.drain(..skip * channels);
    if !block.samples.is_empty() {
        let offset = sample_timestamp(skip as u64, block.sample_rate)
            .ok_or_else(|| RecorderError::invalid("Continuity sample offset overflowed."))?;
        block.timestamp.0 = block
            .timestamp
            .0
            .checked_add(offset.0)
            .ok_or_else(|| RecorderError::invalid("Continuity timestamp overflowed."))?;
    }
    Ok(skip as u64)
}

impl<S: DrainableAudioSession> AudioPipeline<S> {
    /// Idempotent. Tells every open source to finish taking native packets; it never
    /// cancels, stops, joins or discards audio. A repeated request is a no-op and no
    /// source is notified twice. Every source is notified even when an earlier request
    /// failed; the first failure then closes every session and is stored with its code.
    pub fn request_drain(&mut self) -> Result<(), RecorderError> {
        match &self.state {
            State::Running => {}
            State::Draining => return Ok(()),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) if self.drain_complete => return Ok(()),
            State::Terminal(None) => return Err(stopped()),
        }
        self.state = State::Draining;
        let mut first: Option<RecorderError> = None;
        for index in 0..self.sources.len() {
            let expected = self.sources[index].id.clone();
            let request = match self.sources[index].session.as_mut() {
                Some(session) => session.request_drain(),
                None => Err(internal(format!(
                    "Audio source {} lost its session while draining.",
                    expected.0
                ))),
            };
            if let Err(error) = request
                && first.is_none()
            {
                first = Some(error);
            }
        }
        match first {
            Some(error) => Err(self.fail(error)),
            None => Ok(()),
        }
    }

    /// One graceful tail step: at most one block per source, at most one quantum.
    ///
    /// A source that reports `Drained` is handed to the mixer through exactly one
    /// `end_source`; once every source has, the mixer is finished exactly once, before
    /// any further output. A drain never advances media time: before `finish` it polls
    /// the mixer at media time 0, which precedes every quantum deadline, so a passing
    /// wall clock can never turn still-queued audio into a silence quantum and can
    /// never make a queued block look late. The final shorter piece is returned like
    /// any other quantum. `Drained` is persistent and only returned once the mixer
    /// emitted every accepted frame *and* every session is closed; an aborted drain
    /// never reports it. The first failure keeps its code, closes every session and is
    /// repeated by later calls.
    pub fn poll_drain(&mut self) -> Result<PipelineDrainPoll, RecorderError> {
        match &self.state {
            State::Draining => {}
            State::Running => return Err(drain_was_not_requested()),
            State::Terminal(Some(error)) => return Err(error.clone()),
            State::Terminal(None) if self.drain_complete => return Ok(PipelineDrainPoll::Drained),
            State::Terminal(None) => return Err(stopped()),
        }
        for index in 0..self.sources.len() {
            if self.reported_drained[index] {
                continue;
            }
            let expected = self.sources[index].id.clone();
            let verdict = match self.sources[index].session.as_mut() {
                Some(session) => session.poll_drain(),
                None => Err(internal(format!(
                    "Audio source {} lost its session while draining.",
                    expected.0
                ))),
            };
            match verdict {
                Ok(PcmDrainPoll::Block(block)) => self.accept_block(index, block)?,
                // Neither a contended lock nor an unfinished worker is a verdict.
                Ok(PcmDrainPoll::Pending) => {}
                Ok(PcmDrainPoll::Drained) => {
                    // Exactly once per source, and only the mixer decides what the
                    // early end means for its missing-frame accounting.
                    if let Err(error) = self.processor.end_source(&expected) {
                        return Err(self.fail(error));
                    }
                    self.reported_drained[index] = true;
                }
                Err(error) => return Err(self.fail(error)),
            }
        }
        // Admission closes once, only after every source delivered its last block, and
        // never after this point is unread audio admitted: no output is produced first.
        if !self.mixer_finished && self.reported_drained.iter().all(|drained| *drained) {
            if let Err(error) = self.processor.finish() {
                return Err(self.fail(error));
            }
            self.mixer_finished = true;
        }
        // Media time 0 precedes every quantum deadline (deadlines are origin-relative
        // plus the grace period), so the drain never takes the deadline path: a
        // not-yet-covered range stays `Pending` instead of becoming silence.
        let quantum = match self.processor.poll(MediaTimestamp(0)) {
            Ok(quantum) => quantum,
            Err(error) => return Err(self.fail(error)),
        };
        if self.mixer_finished && self.processor.is_drained() {
            // Every accepted frame is out, so every session can be closed and joined
            // without discarding audio. Only then is `Drained` a permanent verdict.
            if let Err(error) = stop_sources(&mut self.sources) {
                return Err(self.fail(error));
            }
            self.drain_complete = true;
            self.state = State::Terminal(None);
        }
        match quantum {
            Some(quantum) => Ok(PipelineDrainPoll::Quantum(quantum)),
            None if self.drain_complete => Ok(PipelineDrainPoll::Drained),
            None => Ok(PipelineDrainPoll::Pending),
        }
    }
}

impl<S: AudioSession> Drop for AudioPipeline<S> {
    fn drop(&mut self) {
        // Session `Drop` is the documented second cleanup attempt.
        let _ = self.stop();
    }
}

/// Cancels and stops every source, attempting all of them, then drops all sessions.
fn stop_sources<S: AudioSession>(sources: &mut [Source<S>]) -> Result<(), RecorderError> {
    let mut first: Option<RecorderError> = None;
    // Signal all workers before any stop can wait for a native join.
    for source in sources.iter() {
        source.cancellation.cancel();
    }
    for source in sources.iter_mut() {
        if let Some(session) = source.session.as_mut() {
            if let Err(error) = session.stop() {
                first = Some(match first.take() {
                    Some(primary) => with_cleanup(primary, Err(error)),
                    None => error,
                });
            }
            source.generation = session.lifecycle().generation();
        }
        // Dropping the session is the second, documented cleanup attempt.
        source.session = None;
    }
    match first {
        Some(error) => Err(error),
        None => Ok(()),
    }
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
        message: "The audio pipeline has been stopped.".into(),
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
        message: "The audio pipeline drain was not requested.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Call request_drain before poll_drain.".into(),
    }
}

fn drain_owns_the_tail() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "The audio pipeline is draining; the live poll is unavailable.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Finish the drain with poll_drain, or abort it with stop.".into(),
    }
}
