// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Bounded, aligned 48 kHz PCM routing with gain and stereo balance on the caller's timeline.

use crate::{PcmBlock, TrackKind, TrackPlan, plan_tracks};
use frame_clock::{MediaTimestamp, sample_timestamp};
use frame_contracts::{AudioRoute, AudioRouteSelection, AudioSourceId, RecorderError};
use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroU32};

const RATE: u32 = 48_000;
const QUANTUM: u64 = 480;
const MAX_BLOCK: usize = 4_800;
const MAX_LOOKAHEAD: u64 = 48_000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const DEADLINE_NS: u64 = 100_000_000;

/// One output track, identified only by its index in `AlignedPcmProcessor::tracks`.
#[derive(Debug, PartialEq)]
pub struct TrackPcm {
    pub track_index: usize,
    pub timestamp: MediaTimestamp,
    pub sample_rate: NonZeroU32,
    pub channels: NonZeroU16,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapCount {
    pub source_id: AudioSourceId,
    pub missing_frames: u16,
}

/// At most one ten-millisecond quantum per call. Clipping counts channel samples.
#[derive(Debug, PartialEq)]
pub struct PcmQuantum {
    pub tracks: Vec<TrackPcm>,
    pub gaps: Vec<GapCount>,
    pub clipped_samples: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    Ignored,
}

struct Pending {
    start: u64,
    frames: u64,
    block: PcmBlock,
}

struct Source {
    id: AudioSourceId,
    route: AudioRoute,
    muted: bool,
    gain: f32,
    left_balance: f32,
    right_balance: f32,
    channels: NonZeroU16,
    pending: VecDeque<Pending>,
    last_end: Option<u64>,
    ended: bool,
    /// Still open when `finish` froze the boundary, so uncovered tail frames are
    /// counted as gaps exactly as they would have been for a live source.
    open_at_finish: bool,
    snaps: u64,
    gaps: u64,
}

/// Fixed routes and bounded input ownership; returned output belongs to the caller.
pub struct AlignedPcmProcessor {
    origin: MediaTimestamp,
    tracks: Vec<TrackPlan>,
    sources: Vec<Source>,
    next_frame: u64,
    /// Frozen tail boundary, only meaningful after `finish`.
    final_frame: u64,
    finished: bool,
    owned_bytes: usize,
    output_reservation: usize,
    clipped_samples: u64,
}

impl AlignedPcmProcessor {
    pub fn new(
        routes: &[AudioRouteSelection],
        origin: MediaTimestamp,
    ) -> Result<Self, RecorderError> {
        // Include frozen metadata, queue slots and the worst output quantum. Admission
        // is a byte budget, independent of application/microphone counts.
        let mut plan_bytes = std::mem::size_of::<TrackPlan>();
        let mut output_reservation = QUANTUM as usize * 2 * 4 * 3 + std::mem::size_of::<TrackPcm>();
        for route in routes {
            let metadata = std::mem::size_of::<Source>()
                + std::mem::size_of::<TrackPlan>()
                + 3 * std::mem::size_of::<AudioSourceId>()
                + if route.route == AudioRoute::Ignore {
                    0
                } else {
                    32 * std::mem::size_of::<Pending>()
                };
            plan_bytes = plan_bytes
                .saturating_add(metadata)
                .saturating_add(route.source_id.0.len().saturating_mul(4));
            output_reservation = output_reservation
                .saturating_add(std::mem::size_of::<Vec<f32>>() + std::mem::size_of::<GapCount>())
                .saturating_add(route.source_id.0.len());
            if matches!(
                route.route,
                AudioRoute::SeparateOnly | AudioRoute::MainMixAndSeparate
            ) {
                output_reservation = output_reservation
                    .saturating_add(QUANTUM as usize * 2 * 4 + std::mem::size_of::<TrackPcm>());
            }
        }
        if plan_bytes.saturating_add(output_reservation) > MAX_BYTES / 2 {
            return Err(RecorderError::invalid(
                "Audio source plan exceeds the PCM memory budget.",
            ));
        }
        for route in routes {
            route.controls.validate()?;
        }
        let tracks = plan_tracks(routes)?;
        let mut sources: Vec<_> = routes
            .iter()
            .map(|route| Source {
                id: route.source_id.clone(),
                route: route.route,
                muted: route.muted,
                gain: 10.0_f32.powf(route.controls.gain_millidb as f32 / 20_000.0),
                left_balance: 1.0 - f32::from(route.controls.balance.max(0)) / 100.0,
                right_balance: 1.0 + f32::from(route.controls.balance.min(0)) / 100.0,
                channels: NonZeroU16::new(2).unwrap(),
                pending: VecDeque::with_capacity(if route.route == AudioRoute::Ignore {
                    0
                } else {
                    32
                }),
                last_end: None,
                ended: false,
                open_at_finish: false,
                snaps: 0,
                gaps: 0,
            })
            .collect();
        sources.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            origin,
            tracks,
            sources,
            next_frame: 0,
            final_frame: 0,
            finished: false,
            owned_bytes: plan_bytes,
            output_reservation,
            clipped_samples: 0,
        })
    }

    pub fn set_muted(&mut self, id: &AudioSourceId, muted: bool) -> Result<(), RecorderError> {
        let index = self
            .sources
            .binary_search_by(|s| s.id.cmp(id))
            .map_err(|_| RecorderError::invalid("Audio source is not configured."))?;
        self.sources[index].muted = muted;
        Ok(())
    }

    pub(crate) fn set_source_channels(
        &mut self,
        id: &AudioSourceId,
        channels: NonZeroU16,
    ) -> Result<(), RecorderError> {
        if !matches!(channels.get(), 1 | 2) {
            return Err(RecorderError::unsupported("Audio channel layout"));
        }
        let index = self
            .sources
            .binary_search_by(|s| s.id.cmp(id))
            .map_err(|_| RecorderError::invalid("Audio source is not configured."))?;
        self.sources[index].channels = channels;
        Ok(())
    }

    pub fn track_channels(&self, index: usize) -> NonZeroU16 {
        match &self.tracks[index].kind {
            TrackKind::MainMix => NonZeroU16::new(2).unwrap(),
            TrackKind::Clean(id) => self.source(id).channels,
        }
    }

    pub fn tracks(&self) -> &[TrackPlan] {
        &self.tracks
    }

    pub fn next_frame(&self) -> u64 {
        self.next_frame
    }

    pub(crate) fn next_timestamp(&self) -> Result<MediaTimestamp, RecorderError> {
        self.frame_timestamp(self.next_frame)
    }

    pub fn clipped_samples(&self) -> u64 {
        self.clipped_samples
    }

    /// Cumulative counters in source-ID order, including ignored sources as zeroes.
    pub fn source_counts(&self) -> impl Iterator<Item = (&AudioSourceId, u64, u64)> {
        self.sources.iter().map(|s| (&s.id, s.snaps, s.gaps))
    }

    /// On failure, the original block and its allocation return to the caller.
    pub fn push(&mut self, block: PcmBlock) -> Result<PushOutcome, (RecorderError, PcmBlock)> {
        // Admission closes on finish, before any source lookup or state change, so
        // the rejected block comes back untouched.
        if self.finished {
            return Err((
                RecorderError::invalid("Audio admission is closed: the processor has finished."),
                block,
            ));
        }
        let Some(index) = self
            .sources
            .binary_search_by(|s| s.id.cmp(&block.source_id))
            .ok()
        else {
            return Err((
                RecorderError::invalid("Audio source is not configured."),
                block,
            ));
        };
        let source = &self.sources[index];
        if source.ended {
            return Err((RecorderError::invalid("Audio source has ended."), block));
        }
        if source.route == AudioRoute::Ignore {
            return Ok(PushOutcome::Ignored);
        }
        if block.sample_rate.get() != RATE || !matches!(block.channels.get(), 1 | 2) {
            return Err((
                RecorderError::unsupported("PCM rate or channel layout"),
                block,
            ));
        }
        let channels = usize::from(block.channels.get());
        if block.samples.is_empty()
            || !block.samples.len().is_multiple_of(channels)
            || block.samples.len() / channels > MAX_BLOCK
            || block.samples.iter().any(|s| !s.is_finite())
        {
            return Err((
                RecorderError::invalid("PCM block has invalid length or non-finite samples."),
                block,
            ));
        }
        let Some(mut start) = self.sample_index(block.timestamp) else {
            return Err((
                RecorderError::invalid("PCM timestamp precedes origin or overflows."),
                block,
            ));
        };
        let original_start = start;
        if let Some(end) = source.last_end
            && end >= self.next_frame
            && start.abs_diff(end) <= 1
        {
            start = end;
        }
        let snapped = start != original_start;
        let frames = u64::try_from(block.samples.len() / channels).expect("bounded block");
        let Some(end) = start.checked_add(frames) else {
            return Err((RecorderError::invalid("PCM sample index overflows."), block));
        };
        if start < self.next_frame
            || self
                .next_frame
                .checked_add(MAX_LOOKAHEAD)
                .is_none_or(|limit| end > limit)
            || source.last_end.is_some_and(|last| start < last)
        {
            return Err((
                RecorderError::invalid(format!(
                    "PCM is late, overlapping, or beyond lookahead: source={} start={start} end={end} next={} previous_end={:?}.",
                    block.source_id.0, self.next_frame, source.last_end
                )),
                block,
            ));
        }
        let Some(bytes) = block
            .samples
            .capacity()
            .checked_mul(std::mem::size_of::<f32>())
            .and_then(|bytes| bytes.checked_add(block.source_id.0.capacity()))
        else {
            return Err((
                RecorderError::invalid("PCM allocation exceeds capacity."),
                block,
            ));
        };
        if source.pending.len() >= 32
            || self
                .owned_bytes
                .checked_add(bytes)
                .and_then(|sum| sum.checked_add(self.output_bytes()))
                .is_none_or(|sum| sum > MAX_BYTES)
        {
            return Err((
                RecorderError::invalid("PCM pending capacity exhausted."),
                block,
            ));
        }
        self.owned_bytes += bytes;
        let source = &mut self.sources[index];
        source.pending.push_back(Pending {
            start,
            frames,
            block,
        });
        source.last_end = Some(end);
        if snapped {
            source.snaps += 1;
        }
        Ok(PushOutcome::Accepted)
    }

    pub fn end_source(&mut self, id: &AudioSourceId) -> Result<(), RecorderError> {
        let index = self
            .sources
            .binary_search_by(|s| s.id.cmp(id))
            .map_err(|_| RecorderError::invalid("Audio source is not configured."))?;
        self.sources[index].ended = true;
        Ok(())
    }

    /// Idempotent finalization for the recording tail. Call only after the caller
    /// has delivered every native PCM block; this module neither drains a device
    /// nor waits for one.
    ///
    /// Closes admission, ends every source and freezes the last boundary at
    /// `max(next_frame, every accepted frame end)`, so ignored sources never
    /// extend it and no frame past the last accepted sample is ever emitted.
    /// Nothing is mutated unless the frozen boundary is representable on the
    /// caller's media timeline.
    pub fn finish(&mut self) -> Result<(), RecorderError> {
        if self.finished {
            return Ok(());
        }
        let mut boundary = self.next_frame;
        for source in &self.sources {
            if source.route != AudioRoute::Ignore
                && let Some(end) = source.last_end
            {
                boundary = boundary.max(end);
            }
        }
        // Fail before closing admission rather than during the tail drain.
        self.frame_timestamp(boundary)?;
        self.finished = true;
        self.final_frame = boundary;
        for source in &mut self.sources {
            source.open_at_finish = source.route != AudioRoute::Ignore && !source.ended;
            source.ended = true;
        }
        Ok(())
    }

    /// True once `finish` has run and every accepted frame has been emitted.
    /// Before `finish` this is always false, including for an empty processor.
    pub fn is_drained(&self) -> bool {
        self.finished && self.next_frame >= self.final_frame
    }

    /// Caller advances media time. No wall clock, sleep, or background work.
    /// Ended sources drain accepted samples, then count as absent; ignored sources
    /// never keep the processor running after all contributing sources end.
    ///
    /// After `finish` the frozen tail is emitted without waiting for a deadline:
    /// at most one quantum per call, the last part may be shorter, already accepted
    /// samples are never dropped or padded past the boundary, and `now` is unused.
    pub fn poll(&mut self, now: MediaTimestamp) -> Result<Option<PcmQuantum>, RecorderError> {
        if self.tracks.is_empty() {
            return Ok(None);
        }
        let frames = if self.finished {
            let remaining = self.final_frame.saturating_sub(self.next_frame);
            if remaining == 0 {
                return Ok(None);
            }
            remaining.min(QUANTUM)
        } else {
            let end = self
                .next_frame
                .checked_add(QUANTUM)
                .ok_or_else(|| RecorderError::invalid("PCM timeline overflows."))?;
            let deadline = self
                .frame_timestamp(end)?
                .0
                .checked_add(DEADLINE_NS)
                .ok_or_else(|| RecorderError::invalid("PCM deadline overflows."))?;
            if self
                .sources
                .iter()
                .filter(|s| s.route != AudioRoute::Ignore)
                .all(|s| s.ended && s.pending.is_empty())
            {
                return Ok(None);
            }
            let due = now.0 >= deadline;
            if !due
                && self
                    .sources
                    .iter()
                    .filter(|s| s.route != AudioRoute::Ignore && !s.ended)
                    .any(|s| !covers(s, self.next_frame, end))
            {
                return Ok(None);
            }
            QUANTUM
        };
        let end = self
            .next_frame
            .checked_add(frames)
            .ok_or_else(|| RecorderError::invalid("PCM timeline overflows."))?;
        let timestamp = self.frame_timestamp(self.next_frame)?;
        // Reserve the entire output budget before consuming any queued input.
        let output_bytes = self.output_bytes();
        if self
            .owned_bytes
            .checked_add(output_bytes)
            .is_none_or(|sum| sum > MAX_BYTES)
        {
            return Err(RecorderError::invalid("PCM output capacity exhausted."));
        }
        let mut tracks = Vec::with_capacity(self.tracks.len());
        let mut mixed = vec![[0.0f64; 2]; frames as usize];
        let mut clean = Vec::with_capacity(self.sources.len());
        let mut gaps = Vec::with_capacity(self.sources.len());
        for source in &self.sources {
            let channels = usize::from(source.channels.get());
            let mut samples = if matches!(
                source.route,
                AudioRoute::SeparateOnly | AudioRoute::MainMixAndSeparate
            ) {
                vec![0.0; frames as usize * channels]
            } else {
                Vec::new()
            };
            let mut covered = 0u16;
            for pending in &source.pending {
                let from = pending.start.max(self.next_frame);
                let to = (pending.start + pending.frames).min(end);
                if from >= to {
                    continue;
                }
                let input_channels = usize::from(pending.block.channels.get());
                covered += (to - from) as u16;
                if source.muted {
                    continue;
                }
                for frame in from..to {
                    let input = (frame - pending.start) as usize * input_channels;
                    let output = (frame - self.next_frame) as usize;
                    let mono = pending.block.samples[input] * source.gain;
                    let left = mono * source.left_balance;
                    let right = pending.block.samples[input + input_channels - 1]
                        * source.gain
                        * source.right_balance;
                    if !samples.is_empty() {
                        samples[output * channels] = if channels == 1 { mono } else { left };
                        if channels == 2 {
                            samples[output * channels + 1] = right;
                        }
                    }
                    if matches!(
                        source.route,
                        AudioRoute::MainMix | AudioRoute::MainMixAndSeparate
                    ) {
                        mixed[output][0] += f64::from(left);
                        mixed[output][1] += f64::from(right);
                    }
                }
            }
            if source.route != AudioRoute::Ignore
                && (!source.ended || source.open_at_finish)
                && covered < frames as u16
            {
                gaps.push(GapCount {
                    source_id: source.id.clone(),
                    missing_frames: frames as u16 - covered,
                });
            }
            clean.push(samples);
        }
        let mut clipped = 0u16;
        for (track_index, plan) in self.tracks.iter().enumerate() {
            let samples = match &plan.kind {
                TrackKind::MainMix => mixed
                    .iter()
                    .flatten()
                    .map(|sum| {
                        if !(-1.0..=1.0).contains(sum) {
                            clipped += 1;
                        }
                        sum.clamp(-1.0, 1.0) as f32
                    })
                    .collect(),
                TrackKind::Clean(id) => {
                    let index = self
                        .sources
                        .binary_search_by(|s| s.id.cmp(id))
                        .expect("planned source");
                    std::mem::take(&mut clean[index])
                }
            };
            tracks.push(TrackPcm {
                track_index,
                timestamp,
                sample_rate: NonZeroU32::new(RATE).unwrap(),
                channels: self.track_channels(track_index),
                samples,
            });
        }
        for source in &mut self.sources {
            if let Ok(index) = gaps.binary_search_by(|gap| gap.source_id.cmp(&source.id)) {
                source.gaps += u64::from(gaps[index].missing_frames);
            }
            while source
                .pending
                .front()
                .is_some_and(|p| p.start + p.frames <= end)
            {
                let pending = source.pending.pop_front().expect("front exists");
                self.owned_bytes -= pending.block.samples.capacity() * std::mem::size_of::<f32>()
                    + pending.block.source_id.0.capacity();
            }
        }
        self.next_frame = end;
        self.clipped_samples += u64::from(clipped);
        Ok(Some(PcmQuantum {
            tracks,
            gaps,
            clipped_samples: clipped,
        }))
    }

    // Reserved at admission so accepted input can always advance through poll.
    fn output_bytes(&self) -> usize {
        self.output_reservation
    }

    fn source(&self, id: &AudioSourceId) -> &Source {
        let index = self
            .sources
            .binary_search_by(|s| s.id.cmp(id))
            .expect("planned source");
        &self.sources[index]
    }

    fn sample_index(&self, timestamp: MediaTimestamp) -> Option<u64> {
        let elapsed = timestamp.0.checked_sub(self.origin.0)?;
        let rounded = (u128::from(elapsed) * u128::from(RATE) + 500_000_000) / 1_000_000_000;
        u64::try_from(rounded).ok()
    }

    fn frame_timestamp(&self, frame: u64) -> Result<MediaTimestamp, RecorderError> {
        let relative = sample_timestamp(frame, NonZeroU32::new(RATE).expect("nonzero rate"))
            .ok_or_else(|| RecorderError::invalid("PCM timeline overflows."))?;
        self.origin
            .0
            .checked_add(relative.0)
            .map(MediaTimestamp)
            .ok_or_else(|| RecorderError::invalid("PCM timeline overflows."))
    }
}

fn covers(source: &Source, start: u64, end: u64) -> bool {
    let mut cursor = start;
    for block in &source.pending {
        if block.start > cursor {
            return false;
        }
        cursor = cursor.max(block.start + block.frames);
        if cursor >= end {
            return true;
        }
    }
    false
}
