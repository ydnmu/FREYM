// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Real native PCM formats on the accepted 48 kHz line (FRAME-0206).
//!
//! The mixer and Opus encoder take 48 kHz interleaved mono/stereo, while native
//! formats may run at 44.1, 48 or 96 kHz. This module is the explicit
//! adapter between the two: [`NormalizingAudioSession`] wraps one accepted
//! [`AudioSession`], and [`NormalizingBackend`] wraps one [`AudioCaptureBackend`], so
//! every started source reaches the existing mixer/Opus line on its native timeline.
//!
//! The rules, in one place:
//!
//! * the target is exactly 48 kHz. Recording sessions preserve mono/stereo layouts;
//!   legacy `new` callers retain stereo output. Matching 48 kHz sources bypass
//!   conversion, keeping samples, block boundaries and timestamps;
//! * 44.1 and 96 kHz sources are resampled by the official `rubato` package (sinc with
//!   fixed input chunks), with a nominal ratio only, every buffer allocated before the
//!   first block is processed and `process_into_buffer` used for every chunk;
//! * the 48 kHz grid is derived from the **first accepted source timestamp**, so no
//!   source invents its own epoch, and every emitted block stays within the accepted
//!   100 ms PCM block ceiling;
//! * the resampler's real latency (`output_delay`) is trimmed, so the first emitted frame
//!   is the source's first accepted frame rather than silence;
//! * a drain zero-fills the partial last chunk to flush exactly that latency, and the
//!   emitted count is capped at the exact accepted duration: the padding is never part of
//!   the output duration, and the real tail is not lost;
//! * source timestamp jitter is absorbed by snapping the block onto the contiguous
//!   source-frame counter within one accepted interval tolerance, while a real
//!   discontinuity is an explicit `SourceLost` error;
//! * an unsupported rate or channel layout, a mid-session format change, a non-finite
//!   sample, an oversized block and a real source discontinuity are all explicit errors.
//!   Nothing is silently repaired, dropped or routed through a fallback.
//!
//! The default ratio is nominal. Explicit drift compensation uses the same sinc
//! resampler, requires three reliable clock windows, and changes the ratio gradually
//! within 1000 ppm. It never shifts the first timestamp or the 48 kHz output grid.
//!
//! Memory stays far below the 32 MiB adapter budget: one input chunk, one output chunk,
//! the pending accumulator and at most [`MAX_READY_BLOCKS`] ready blocks of at most
//! 100 ms each are all that is ever held.

use crate::{
    AudioBackend, AudioCaptureBackend, AudioContext, AudioSession, DrainableAudioSession, PcmBlock,
    PcmDrainPoll,
};
use frame_clock::{MediaTimestamp, sample_timestamp};
use frame_contracts::{AudioSource, AudioSourceId, ErrorCode, RecorderError, RecoveryStatus};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::collections::VecDeque;
use std::mem::size_of;
use std::num::{NonZeroU16, NonZeroU32};

/// The rate every accepted route is connected to.
pub const NORMALIZED_RATE: u32 = 48_000;
/// Stereo default retained for legacy callers; recording output preserves channels.
pub const NORMALIZED_CHANNELS: u16 = 2;
/// Emitted block size: 10 ms, one accepted mixer quantum.
pub const OUTPUT_BLOCK_FRAMES: usize = 480;
/// The accepted 100 ms PCM block ceiling, in 48 kHz stereo frames.
pub const MAX_BLOCK_FRAMES: usize = 4_800;
/// Bounded output queue: at most this many ready blocks are ever held.
pub const MAX_READY_BLOCKS: usize = 64;
/// The adapter's whole memory budget.
pub const ADAPTER_MEMORY_BUDGET: usize = 32 * 1024 * 1024;
/// Accepted skew of one source interval. It mirrors the accepted loopback proof
/// tolerance (`application_audio::validate_qpc_interval`), so ordinary endpoint jitter
/// and nominal device clock drift are absorbed while a lost buffer is not.
pub const SOURCE_INTERVAL_TOLERANCE_NS: u64 = 1_000_000;
/// Bounded zero-chunk flush guard of one drain; a defect fails instead of spinning.
const MAX_FLUSH_CHUNKS: usize = 4;

/// Bytes of one maximum-size 48 kHz stereo block.
const MAX_BLOCK_BYTES: usize = MAX_BLOCK_FRAMES * NORMALIZED_CHANNELS as usize * size_of::<f32>();
/// The ready queue alone is proven to fit the adapter budget at compile time.
const _: () = assert!(MAX_READY_BLOCKS * MAX_BLOCK_BYTES < ADAPTER_MEMORY_BUDGET);

/// The exact 48 kHz grid of one supported source rate: `(numerator, denominator)` with
/// `frames_48k == source_frames * numerator / denominator`. `None` is an unsupported
/// rate, never a fallback.
fn target_grid(rate: u32) -> Option<(u64, u64)> {
    match rate {
        44_100 => Some((160, 147)),
        48_000 => Some((1, 1)),
        96_000 => Some((1, 2)),
        _ => None,
    }
}

/// The accepted 100 ms source-frame bound of one native packet, without rounding down.
fn source_frame_cap(rate: u32) -> u64 {
    u64::from(rate).div_ceil(10)
}

/// One accepted source interval tolerance, in source frames.
fn interval_tolerance_frames(rate: u32) -> u64 {
    let frames =
        (u128::from(SOURCE_INTERVAL_TOLERANCE_NS) * u128::from(rate) + 500_000_000) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX).max(1)
}

/// The exact 48 kHz frame count of an accepted source duration, rounded to the nearest
/// frame. Rounding never invents more than half a frame of duration.
fn expected_output_frames(
    source_frames: u64,
    numerator: u64,
    denominator: u64,
) -> Result<u64, RecorderError> {
    let scaled = u128::from(source_frames)
        .checked_mul(u128::from(numerator))
        .ok_or_else(|| internal("The 48 kHz duration overflows.".to_string()))?;
    let rounded = (scaled + u128::from(denominator) / 2) / u128::from(denominator);
    u64::try_from(rounded).map_err(|_| internal("The 48 kHz duration is not addressable.".into()))
}

/// The media time of one emitted 48 kHz frame, derived from the first accepted source
/// timestamp. Nothing here consults a wall clock.
fn output_timestamp(
    first: Option<MediaTimestamp>,
    frame: u64,
) -> Result<MediaTimestamp, RecorderError> {
    let first = first.ok_or_else(|| {
        internal("A 48 kHz block was formed before a source timestamp was accepted.".into())
    })?;
    let rate = NonZeroU32::new(NORMALIZED_RATE).expect("nonzero normalized rate");
    let relative = sample_timestamp(frame, rate)
        .ok_or_else(|| internal("The 48 kHz timeline overflows.".into()))?;
    first
        .0
        .checked_add(relative.0)
        .map(MediaTimestamp)
        .ok_or_else(|| internal("The 48 kHz timeline overflows.".into()))
}

/// The source-frame position a block's own timestamp claims, relative to the first
/// accepted source timestamp. `None` means the timestamp precedes that anchor.
fn source_position(anchor: MediaTimestamp, timestamp: MediaTimestamp, rate: u32) -> Option<u64> {
    let elapsed = timestamp.0.checked_sub(anchor.0)?;
    let rounded = (u128::from(elapsed) * u128::from(rate) + 500_000_000) / 1_000_000_000;
    u64::try_from(rounded).ok()
}

fn unsupported(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Select a source whose mix format is mono or stereo at 44.1, 48 or 96 kHz.".into(),
    }
}

fn source_lost(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Restart capture for this source; lost PCM is never reconstructed.".into(),
    }
}

fn internal(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "This is a defect in the 48 kHz normalization adapter.".into(),
    }
}

fn stopped() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "The normalized audio session has been stopped.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new audio session for the next recording.".into(),
    }
}

/// Keeps the primary failure and its code while preserving the cleanup failure.
fn with_cleanup(mut error: RecorderError, cleanup: RecorderError) -> RecorderError {
    error.message = format!("{} Cleanup also failed: {}", error.message, cleanup.message);
    error
}

/// The native format one session is normalizing, as observed on its first accepted block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFormat {
    pub sample_rate: NonZeroU32,
    pub channels: NonZeroU16,
}

/// What the adapter actually did, for diagnostics and evidence. Every field counts
/// frames or blocks; none of them is derived from a nominal assumption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NormalizeCounters {
    pub blocks_in: u64,
    pub frames_in: u64,
    pub blocks_out: u64,
    pub frames_out: u64,
    /// Source-frame skew between a block's own timestamp and the contiguous source
    /// counter. A resampled stream absorbs it by snapping onto the derived 48 kHz grid; a
    /// 48 kHz bypass keeps its own timestamps and only measures it. Device clock drift
    /// accumulates here rather than being corrected.
    pub snapped_frames: u64,
    /// The resampler's real latency, removed from the front of the 48 kHz stream.
    pub delay_frames_trimmed: u64,
    /// Zero padding produced by the drain flush and excluded from the output duration.
    pub padding_frames_dropped: u64,
}

/// The bypass decision: a 48 kHz source keeps its own samples and timestamps.
enum Stage {
    /// No block has been accepted yet, so the native format is still unknown.
    Unstarted,
    /// 48 kHz input: no conversion, only channel mapping.
    Direct,
    /// 44.1/96 kHz input: the nominal 48 kHz conversion.
    Resampled(Box<Resampler48>),
}

/// Fixed input chunks at a nominal ratio to 48 kHz, with streaming buffers allocated up front.
struct Resampler48 {
    channels: usize,
    resampler: Async<f32>,
    /// Interleaved source frames waiting for a whole input chunk.
    input: Vec<f32>,
    input_chunk_frames: usize,
    input_fill: usize,
    /// One fixed resampler output chunk in the configured channel layout.
    output: Vec<f32>,
    output_capacity_frames: usize,
    /// 48 kHz frames produced and not yet handed to a block.
    pending: Vec<f32>,
    /// Frames produced since the first chunk, after the delay trim.
    available_frames: u64,
    /// The real resampler latency still to be discarded, in output frames.
    delay_remaining: usize,
    trimmed_frames: u64,
}

impl Resampler48 {
    fn new(rate: u32, compensate: bool, channels: usize) -> Result<Self, RecorderError> {
        let rate_usize = usize::try_from(rate)
            .map_err(|_| unsupported(format!("The {rate} Hz source rate is not addressable.")))?;
        let sinc = SincInterpolationParameters::new(128, WindowFunction::Blackman2)
            .oversampling_factor(256)
            .interpolation(SincInterpolationType::Quadratic);
        let resampler = Async::<f32>::new_sinc(
            NORMALIZED_RATE as f64 / rate_usize as f64,
            if compensate { 1.002 } else { 1.0 },
            &sinc,
            OUTPUT_BLOCK_FRAMES,
            channels,
            FixedAsync::Input,
        )
        .map_err(|error| {
            internal(format!(
                "A 48 kHz resampler for a {rate} Hz source could not be created: {error}."
            ))
        })?;
        let input_chunk_frames = resampler.input_frames_next();
        let output_chunk_frames = resampler.output_frames_next();
        let output_capacity_frames = resampler.output_frames_max().max(output_chunk_frames);
        // Enough room for the residue of one block, the zero-filled last chunk and the
        // bounded flush, so processing never grows or overflows a buffer.
        let pending_capacity_frames =
            (MAX_FLUSH_CHUNKS + 2) * output_capacity_frames + OUTPUT_BLOCK_FRAMES;
        let owned_bytes = (input_chunk_frames + output_capacity_frames + pending_capacity_frames)
            * channels
            * size_of::<f32>();
        if owned_bytes + MAX_READY_BLOCKS * MAX_BLOCK_BYTES > ADAPTER_MEMORY_BUDGET {
            return Err(internal(format!(
                "The 48 kHz adapter would hold {owned_bytes} bytes of streaming buffers beside its bounded output queue, above the {ADAPTER_MEMORY_BUDGET} byte budget."
            )));
        }
        if input_chunk_frames == 0 || output_chunk_frames == 0 {
            return Err(internal(
                "The 48 kHz resampler reported an empty fixed chunk.".into(),
            ));
        }
        let delay_remaining = resampler.output_delay();
        Ok(Self {
            channels,
            resampler,
            // Pre-allocated once: processing never grows a buffer.
            input: vec![0.0; input_chunk_frames * channels],
            input_chunk_frames,
            input_fill: 0,
            output: vec![0.0; output_capacity_frames * channels],
            output_capacity_frames,
            pending: Vec::with_capacity(pending_capacity_frames * channels),
            available_frames: 0,
            delay_remaining,
            trimmed_frames: 0,
        })
    }

    fn set_relative_ratio(&mut self, relative: f64) -> Result<(), RecorderError> {
        self.resampler
            .set_resample_ratio_relative(relative, true)
            .map_err(|error| internal(format!("Clock compensation ratio was refused: {error}.")))
    }

    fn pending_frames(&self) -> usize {
        let channels = self.channels;
        self.pending.len() / channels
    }

    /// Copies one whole native block into the staged channel layout and processes
    /// every complete chunk it fills.
    fn push(&mut self, samples: &[f32], input_channels: u16) -> Result<(), RecorderError> {
        let channels = self.channels;
        let input_channels = usize::from(input_channels);
        let frames = samples.len() / input_channels;
        let mut consumed = 0;
        while consumed < frames {
            let take = (self.input_chunk_frames - self.input_fill).min(frames - consumed);
            for index in 0..take {
                for channel in 0..channels {
                    self.input[(self.input_fill + index) * channels + channel] = samples
                        [(consumed + index) * input_channels + channel.min(input_channels - 1)];
                }
            }
            self.input_fill += take;
            consumed += take;
            if self.input_fill == self.input_chunk_frames {
                self.process_chunk()?;
            }
        }
        Ok(())
    }

    /// One whole fixed chunk through the pre-allocated output buffer.
    fn process_chunk(&mut self) -> Result<(), RecorderError> {
        if self.resampler.input_frames_next() != self.input_chunk_frames {
            return Err(internal(
                "The 48 kHz resampler changed its fixed input chunk.".into(),
            ));
        }
        let expected_output = self.resampler.output_frames_next();
        if expected_output > self.output_capacity_frames {
            return Err(internal(
                "The 48 kHz resampler output no longer fits its reserved buffer.".into(),
            ));
        }
        let produced = {
            let channels = self.channels;
            let input_frames = self.input_chunk_frames;
            let output_frames = self.output_capacity_frames;
            let input_adapter =
                InterleavedSlice::new(self.input.as_slice(), channels, input_frames).map_err(
                    |error| internal(format!("The resampler input view was refused: {error}.")),
                )?;
            let mut output_adapter =
                InterleavedSlice::new_mut(self.output.as_mut_slice(), channels, output_frames)
                    .map_err(|error| {
                        internal(format!("The resampler output view was refused: {error}."))
                    })?;
            let (consumed, produced) = self
                .resampler
                .process_into_buffer(&input_adapter, &mut output_adapter, None)
                .map_err(|error| internal(format!("The 48 kHz resampler failed: {error}.")))?;
            if consumed != input_frames {
                return Err(internal(format!(
                    "The 48 kHz resampler consumed {consumed} of {input_frames} input frames."
                )));
            }
            if produced != expected_output {
                return Err(internal(format!(
                    "The 48 kHz resampler produced {produced} frames where {expected_output} were promised."
                )));
            }
            produced
        };
        self.input_fill = 0;
        self.accumulate(produced)
    }

    /// Moves one produced chunk into the pending buffer, discarding exactly the
    /// resampler's real startup latency from the front.
    fn accumulate(&mut self, produced: usize) -> Result<(), RecorderError> {
        let mut start = 0usize;
        if self.delay_remaining > 0 {
            let skip = self.delay_remaining.min(produced);
            self.delay_remaining -= skip;
            self.trimmed_frames += skip as u64;
            start = skip;
        }
        let frames = produced - start;
        if frames == 0 {
            return Ok(());
        }
        let channels = self.channels;
        if self.pending.len() + frames * channels > self.pending.capacity() {
            return Err(internal(
                "The 48 kHz output accumulator exceeded its reserved capacity.".into(),
            ));
        }
        // Preserve the library's sinc output. Its whole-frame delay leaves at
        // most one output sample of phase offset; a linear fractional-delay
        // filter here would attenuate audible high frequencies.
        self.pending
            .extend_from_slice(&self.output[start * channels..produced * channels]);
        self.available_frames += frames as u64;
        Ok(())
    }

    /// Zero-fills the partial last chunk and pushes the real latency out until at least
    /// `needed_frames` accepted 48 kHz frames exist. The caller caps what it stages, so
    /// the zero padding can never extend the output duration.
    fn flush_to(&mut self, needed_frames: u64) -> Result<(), RecorderError> {
        let channels = self.channels;
        if self.input_fill > 0 {
            let fill = self.input_fill;
            self.input[fill * channels..].fill(0.0);
            self.input_fill = self.input_chunk_frames;
            self.process_chunk()?;
        }
        let mut guard = MAX_FLUSH_CHUNKS;
        while self.available_frames < needed_frames {
            if guard == 0 {
                return Err(internal(format!(
                    "The 48 kHz resampler did not reach the accepted duration {needed_frames} within {MAX_FLUSH_CHUNKS} flush chunks."
                )));
            }
            guard -= 1;
            self.input.fill(0.0);
            self.input_fill = self.input_chunk_frames;
            self.process_chunk()?;
        }
        Ok(())
    }

    /// Takes whole frames from the front of the pending buffer.
    fn take_frames(&mut self, frames: usize) -> Vec<f32> {
        let channels = self.channels;
        self.pending.drain(..frames * channels).collect()
    }

    /// Drops every pending frame the accepted duration does not cover.
    fn drop_padding(&mut self) -> u64 {
        let dropped = self.pending_frames() as u64;
        self.pending.clear();
        dropped
    }
}

/// One accepted native session, normalized onto the 48 kHz line.
///
/// The session reads at most one native block per call and returns at most one 48 kHz
/// block, so no caller needs a queue. Draining follows the accepted two-phase contract:
/// every already-published native block is still read out, and only then is the tail
/// flushed and capped. A failure is sticky: it is repeated by every later call instead of
/// being hidden behind buffered audio.
pub struct NormalizingAudioSession<S: AudioSession> {
    output_channels: NonZeroU16,
    inner: S,
    source_id: AudioSourceId,
    stage: Stage,
    format: Option<SourceFormat>,
    first_timestamp: Option<MediaTimestamp>,
    segment_frames_out: u64,
    /// The contiguous source-frame counter: the truth for the resampled stream.
    source_frames: u64,
    /// Raw source timestamps are observed before nominal rate conversion can
    /// hide clock-speed error by snapping to its output grid.
    clock_drift: frame_clock::ClockDriftEstimator,
    compensation: Option<crate::policy::DriftController>,
    /// Integrated duration under the actual controlled ratios, in 48 kHz frames.
    corrected_frames: f64,
    /// The end position the previous block's own timestamp claimed.
    last_source_end: Option<u64>,
    ready: VecDeque<PcmBlock>,
    counters: NormalizeCounters,
    drain_requested: bool,
    inner_drained: bool,
    finalized: bool,
    stopped: bool,
    stop_result: Option<RecorderError>,
    continuity_cleanup_error: Option<RecorderError>,
    failure: Option<RecorderError>,
}

impl<S: AudioSession> NormalizingAudioSession<S> {
    /// Wraps one started session. `source_id` is the identity it was started for, so a
    /// block tagged with anything else is refused instead of being merged.
    pub fn new(inner: S, source_id: AudioSourceId) -> Self {
        Self {
            output_channels: NonZeroU16::new(2).unwrap(),
            inner,
            source_id,
            stage: Stage::Unstarted,
            format: None,
            first_timestamp: None,
            segment_frames_out: 0,
            source_frames: 0,
            clock_drift: frame_clock::ClockDriftEstimator::default(),
            compensation: None,
            corrected_frames: 0.0,
            last_source_end: None,
            ready: VecDeque::new(),
            counters: NormalizeCounters::default(),
            drain_requested: false,
            inner_drained: false,
            finalized: false,
            stopped: false,
            stop_result: None,
            continuity_cleanup_error: None,
            failure: None,
        }
    }

    pub fn preserving_channels(mut self) -> Self {
        self.output_channels = self.inner.pcm_channels();
        self
    }

    pub fn with_drift_compensation(mut self, enabled: bool) -> Self {
        if enabled {
            self.compensation = Some(crate::policy::DriftController::default());
        }
        self
    }

    /// The native format observed on the first accepted block, if any.
    pub fn source_format(&self) -> Option<SourceFormat> {
        self.format
    }

    /// True once the session is connected to the resampling stage.
    pub fn is_resampling(&self) -> bool {
        matches!(self.stage, Stage::Resampled(_))
    }

    /// What the adapter actually did; never a nominal expectation.
    pub fn counters(&self) -> NormalizeCounters {
        self.counters
    }

    fn pop_ready(&mut self) -> Option<PcmBlock> {
        self.ready.pop_front()
    }

    /// Stores the first failure and keeps its code; later failures never replace it.
    fn fail(&mut self, error: RecorderError) -> RecorderError {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.failure.clone().expect("just stored")
    }

    fn push_ready(&mut self, block: PcmBlock) -> Result<(), RecorderError> {
        if self.ready.len() >= MAX_READY_BLOCKS {
            return Err(self.fail(RecorderError::invalid(format!(
                "The 48 kHz adapter holds {MAX_READY_BLOCKS} unread blocks for {}; the consumer is not reading them.",
                self.source_id.0
            ))));
        }
        self.ready.push_back(block);
        Ok(())
    }

    /// One accepted native block, validated and connected to the 48 kHz line.
    fn ingest(&mut self, block: PcmBlock) -> Result<(), RecorderError> {
        match self.accept(block) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.fail(error)),
        }
    }

    fn accept(&mut self, block: PcmBlock) -> Result<(), RecorderError> {
        if block.source_id != self.source_id {
            return Err(RecorderError::invalid(format!(
                "Audio source {} returned PCM tagged as {}.",
                self.source_id.0, block.source_id.0
            )));
        }
        let rate = block.sample_rate.get();
        let channels = block.channels.get();
        if target_grid(rate).is_none() {
            return Err(unsupported(format!(
                "A {rate} Hz source cannot be connected to the {NORMALIZED_RATE} Hz line; only 44100, 48000 and 96000 Hz are connected."
            )));
        }
        if !matches!(channels, 1 | 2) {
            return Err(unsupported(format!(
                "A {channels}-channel source cannot be connected to the {NORMALIZED_RATE} Hz mono/stereo line."
            )));
        }
        let observed = SourceFormat {
            sample_rate: block.sample_rate,
            channels: block.channels,
        };
        match self.format {
            None => self.format = Some(observed),
            Some(known) if known == observed => {}
            Some(known) => {
                return Err(source_lost(format!(
                    "Audio source {} changed its native format from {} Hz/{} channels to {rate} Hz/{channels} channels mid-session.",
                    self.source_id.0, known.sample_rate, known.channels
                )));
            }
        }
        let channel_count = usize::from(channels);
        if block.samples.is_empty() || !block.samples.len().is_multiple_of(channel_count) {
            return Err(RecorderError::invalid(format!(
                "Audio source {} returned a PCM block with an invalid length.",
                self.source_id.0
            )));
        }
        if block.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(RecorderError::invalid(format!(
                "Audio source {} returned a non-finite sample; the block is refused instead of repaired.",
                self.source_id.0
            )));
        }
        let frames = (block.samples.len() / channel_count) as u64;
        let cap = source_frame_cap(rate);
        if frames > cap {
            return Err(RecorderError::invalid(format!(
                "Audio source {} returned a {frames}-frame native block, above the {cap} frame (100 ms) source bound.",
                self.source_id.0
            )));
        }
        // The first accepted block fixes the epoch; every later position is measured
        // against it. A resampled stream is then snapped onto the contiguous source-frame
        // counter, so jitter and nominal drift never become a gap and the resampler's own
        // latency is never mistaken for one; a 48 kHz bypass keeps its own timestamps and
        // only measures the same skew.
        let (position, first) = match self.first_timestamp {
            Some(anchor) => (
                source_position(anchor, block.timestamp, rate).ok_or_else(|| {
                    source_lost(format!(
                        "Audio source {} reported {} before its first accepted source time {}.",
                        self.source_id.0, block.timestamp.0, anchor.0
                    ))
                })?,
                false,
            ),
            None => (0, true),
        };
        let mut skew = 0u64;
        if let Some(previous_end) = self.last_source_end {
            let tolerance = interval_tolerance_frames(rate);
            skew = position.abs_diff(previous_end);
            if skew > tolerance && position > previous_end && self.inner.allows_silent_gaps() {
                self.finalize()?;
                self.stage = Stage::Unstarted;
                self.first_timestamp = None;
                self.segment_frames_out = 0;
                self.source_frames = 0;
                self.corrected_frames = 0.0;
                self.last_source_end = None;
                self.finalized = false;
                self.clock_drift = Default::default();
                if self.compensation.is_some() {
                    self.compensation = Some(Default::default());
                }
                return self.accept(block);
            }
            if skew > tolerance {
                return Err(source_lost(format!(
                    "Audio source {} reported source frame {position} where {previous_end} was expected: {skew} frames are outside the {tolerance} frame ({SOURCE_INTERVAL_TOLERANCE_NS} ns) interval tolerance, so PCM was lost rather than jittered.",
                    self.source_id.0
                )));
            }
        }
        self.last_source_end = Some(position.saturating_add(frames));
        if first {
            self.first_timestamp = Some(block.timestamp);
        }
        self.counters.blocks_in += 1;
        self.counters.frames_in += frames;
        let estimate =
            self.clock_drift
                .observe(self.source_frames, block.sample_rate, block.timestamp);
        let relative = if let Some(controller) = &mut self.compensation {
            let elapsed = block.timestamp.0 - self.first_timestamp.expect("accepted anchor").0;
            let residual_ns =
                self.corrected_frames * 1_000_000_000.0 / NORMALIZED_RATE as f64 - elapsed as f64;
            controller.update(block.timestamp, estimate, residual_ns)
        } else {
            1.0
        };
        self.corrected_frames += frames as f64 * NORMALIZED_RATE as f64 / rate as f64 * relative;
        self.source_frames = self.source_frames.saturating_add(frames);
        if matches!(self.stage, Stage::Unstarted) {
            self.stage = if rate == NORMALIZED_RATE && self.compensation.is_none() {
                Stage::Direct
            } else {
                Stage::Resampled(Box::new(Resampler48::new(
                    rate,
                    self.compensation.is_some(),
                    usize::from(self.output_channels.get()),
                )?))
            };
        }
        if matches!(self.stage, Stage::Resampled(_)) {
            self.counters.snapped_frames += skew;
        }
        match &mut self.stage {
            Stage::Direct => self.emit_direct(block),
            Stage::Resampled(resampler) => {
                if self.compensation.is_some() {
                    resampler.set_relative_ratio(relative)?;
                }
                // Consume each completed input chunk before admitting the next.
                // A valid 100 ms native packet must fit without expanding buffers.
                let channels = block.channels.get();
                let chunk_samples = resampler.input_chunk_frames * usize::from(channels);
                for chunk in block.samples.chunks(chunk_samples) {
                    if let Stage::Resampled(resampler) = &mut self.stage {
                        resampler.push(chunk, channels)?;
                    }
                    self.form_blocks(None)?;
                }
                Ok(())
            }
            Stage::Unstarted => Err(internal(
                "The 48 kHz adapter accepted a block without choosing a stage.".into(),
            )),
        }
    }

    /// 48 kHz input: the native samples and timestamps are forwarded, with mono
    /// duplicated into both channels.
    ///
    /// The accepted source bound already keeps the block inside the 100 ms ceiling, so one
    /// native block stays one normalized block and nothing is re-timed.
    fn emit_direct(&mut self, block: PcmBlock) -> Result<(), RecorderError> {
        let channels = usize::from(block.channels.get());
        let frames = block.samples.len() / channels;
        debug_assert!(frames <= MAX_BLOCK_FRAMES);
        // Stereo input keeps its own allocation; mono is duplicated into both channels.
        let samples = if channels == usize::from(self.output_channels.get()) {
            block.samples
        } else {
            let mut stereo = Vec::with_capacity(frames * usize::from(self.output_channels.get()));
            for sample in &block.samples {
                stereo.push(*sample);
                stereo.push(*sample);
            }
            stereo
        };
        self.push_ready(PcmBlock {
            source_id: block.source_id,
            timestamp: block.timestamp,
            sample_rate: NonZeroU32::new(NORMALIZED_RATE).expect("nonzero normalized rate"),
            channels: self.output_channels,
            samples,
        })?;
        self.counters.frames_out += frames as u64;
        self.segment_frames_out += frames as u64;
        self.counters.blocks_out += 1;
        Ok(())
    }

    /// Forms 48 kHz blocks from the resampler's pending frames, optionally capped at the
    /// accepted duration known after a drain.
    fn form_blocks(&mut self, limit: Option<u64>) -> Result<(), RecorderError> {
        let Self {
            output_channels,
            stage,
            ready,
            counters,
            source_id,
            first_timestamp,
            segment_frames_out,
            ..
        } = self;
        let Stage::Resampled(resampler) = stage else {
            return Ok(());
        };
        counters.delay_frames_trimmed = resampler.trimmed_frames;
        let channels = *output_channels;
        let sample_rate = NonZeroU32::new(NORMALIZED_RATE).expect("nonzero normalized rate");
        loop {
            let available = resampler.pending_frames();
            if available == 0 {
                break;
            }
            let allowed = match limit {
                Some(limit) => limit.saturating_sub(*segment_frames_out),
                None => u64::MAX,
            };
            if allowed == 0 {
                break;
            }
            let take = available
                .min(OUTPUT_BLOCK_FRAMES)
                .min(usize::try_from(allowed).unwrap_or(usize::MAX));
            let timestamp = output_timestamp(*first_timestamp, *segment_frames_out)?;
            let samples = resampler.take_frames(take);
            if ready.len() >= MAX_READY_BLOCKS {
                return Err(internal(format!(
                    "The 48 kHz adapter holds {MAX_READY_BLOCKS} unread blocks for {} while resampling.",
                    source_id.0
                )));
            }
            ready.push_back(PcmBlock {
                source_id: source_id.clone(),
                timestamp,
                sample_rate,
                channels,
                samples,
            });
            counters.frames_out += take as u64;
            *segment_frames_out += take as u64;
            counters.blocks_out += 1;
        }
        Ok(())
    }

    /// Closes the resampled stream once the source has drained: the partial last chunk is
    /// zero-filled only to flush the real latency, and the emitted count is the exact
    /// accepted duration, so padding never extends the track.
    fn finalize(&mut self) -> Result<(), RecorderError> {
        if self.finalized {
            return Ok(());
        }
        if let Stage::Resampled(_) = &self.stage {
            let rate = self
                .format
                .map(|format| format.sample_rate.get())
                .ok_or_else(|| {
                    internal("A resampled stream has no observed source rate.".into())
                })?;
            let (numerator, denominator) = target_grid(rate).ok_or_else(|| {
                internal(format!(
                    "The observed source rate {rate} Hz lost its target grid."
                ))
            })?;
            let expected = if self.compensation.is_some() {
                self.corrected_frames.round() as u64
            } else {
                expected_output_frames(self.source_frames, numerator, denominator)?
            };
            if self.segment_frames_out > expected {
                return Err(internal(format!(
                    "The 48 kHz adapter staged {} frames for an accepted duration of {expected}.",
                    self.segment_frames_out
                )));
            }
            if let Stage::Resampled(resampler) = &mut self.stage {
                resampler.flush_to(expected)?;
                self.counters.delay_frames_trimmed = resampler.trimmed_frames;
            }
            self.form_blocks(Some(expected))?;
            if let Stage::Resampled(resampler) = &mut self.stage {
                self.counters.padding_frames_dropped = resampler.drop_padding();
            }
        }
        self.finalized = true;
        Ok(())
    }

    /// Returns the stored failure without touching the inner session, if any.
    fn stored_failure(&self) -> Option<RecorderError> {
        self.failure.clone()
    }
}

impl<S: AudioSession> AudioSession for NormalizingAudioSession<S> {
    fn allows_silent_gaps(&self) -> bool {
        self.inner.allows_silent_gaps()
    }
    fn pcm_channels(&self) -> NonZeroU16 {
        self.output_channels
    }
    fn compensation_health(&self) -> Option<crate::DriftCompensationHealth> {
        self.compensation
            .as_ref()
            .map(crate::policy::DriftController::health)
    }
    fn clock_drift(&self) -> Option<frame_clock::DriftEstimate> {
        self.clock_drift.estimate()
    }

    /// Nonblocking. A sticky failure takes precedence over buffered 48 kHz blocks, so a
    /// failed source can never look like a short but healthy one.
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if let Some(error) = self.stored_failure() {
            return Err(error);
        }
        if let Some(block) = self.pop_ready() {
            return Ok(Some(block));
        }
        if self.stopped {
            return Ok(None);
        }
        match self.inner.try_pcm() {
            Ok(None) => Ok(None),
            Ok(Some(block)) => {
                self.ingest(block)?;
                Ok(self.pop_ready())
            }
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Idempotent cleanup of the wrapped session. A stored capture failure and a cleanup
    /// failure are both preserved; neither is reported as a successful stop.
    fn stop(&mut self) -> Result<(), RecorderError> {
        if self.stopped {
            return match &self.stop_result {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            };
        }
        self.stopped = true;
        let cleanup = self.inner.stop();
        // An already-published capture error may itself include a cleanup
        // failure. Equality or SourceLost alone cannot prove native release;
        // require the backend's explicit continuity cleanup contract instead.
        self.continuity_cleanup_error = self.inner.stop_for_continuity().err();
        let result = match (self.failure.clone(), cleanup) {
            (None, Ok(())) => Ok(()),
            (Some(primary), Ok(())) => Err(primary),
            (None, Err(cleanup)) => Err(cleanup),
            (Some(primary), Err(cleanup)) => Err(with_cleanup(primary, cleanup)),
        };
        if let Err(error) = &result {
            self.stop_result = Some(error.clone());
        }
        result
    }

    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        let _ = self.stop();
        if let Some(error) = &self.continuity_cleanup_error {
            return Err(error.clone());
        }
        match &self.failure {
            Some(error) if error.code != ErrorCode::SourceLost => Err(error.clone()),
            _ => Ok(()),
        }
    }
}

impl<S: DrainableAudioSession> DrainableAudioSession for NormalizingAudioSession<S> {
    /// Idempotent and nonblocking: the request is forwarded exactly once and never
    /// cancels, joins or discards audio. Already-normalized blocks stay readable.
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        if let Some(error) = self.stored_failure() {
            return Err(error);
        }
        if self.drain_requested {
            return Ok(());
        }
        match self.inner.request_drain() {
            Ok(()) => {
                self.drain_requested = true;
                Ok(())
            }
            Err(error) => Err(self.fail(error)),
        }
    }

    /// At most one 48 kHz block per call. Already-normalized blocks are handed over
    /// first; the resampler is finalized exactly once, and only after the inner session
    /// reported its own completed drain, so no accepted source frame is ever dropped.
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        if let Some(error) = self.stored_failure() {
            return Err(error);
        }
        if let Some(block) = self.pop_ready() {
            return Ok(PcmDrainPoll::Block(block));
        }
        if self.stopped {
            return Err(stopped());
        }
        for _ in 0..MAX_READY_BLOCKS {
            if self.inner_drained {
                if let Err(error) = self.finalize() {
                    return Err(self.fail(error));
                }
                return match self.pop_ready() {
                    Some(block) => Ok(PcmDrainPoll::Block(block)),
                    None => Ok(PcmDrainPoll::Drained),
                };
            }
            match self.inner.poll_drain() {
                Ok(PcmDrainPoll::Block(block)) => {
                    self.ingest(block)?;
                    if let Some(block) = self.pop_ready() {
                        return Ok(PcmDrainPoll::Block(block));
                    }
                }
                // A contended lock or an unfinished worker is never a verdict.
                Ok(PcmDrainPoll::Pending) => return Ok(PcmDrainPoll::Pending),
                Ok(PcmDrainPoll::Drained) => self.inner_drained = true,
                Err(error) => return Err(self.fail(error)),
            }
        }
        Ok(PcmDrainPoll::Pending)
    }
}

impl<S: AudioSession> Drop for NormalizingAudioSession<S> {
    fn drop(&mut self) {
        // The wrapped session's own `Drop` is the documented second cleanup attempt.
        let _ = self.stop();
    }
}

/// One capture backend whose every started source is connected to the 48 kHz line.
///
/// It adds no capability and hides none: discovery is delegated unchanged, and each
/// session is the accepted session of the wrapped backend behind
/// [`NormalizingAudioSession`].
pub struct NormalizingBackend<B> {
    inner: B,
    policy: crate::AudioContinuityPolicy,
    recording: bool,
    source_controls:
        std::collections::BTreeMap<AudioSourceId, frame_contracts::AudioSourceControls>,
}

impl<B> NormalizingBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            policy: crate::AudioContinuityPolicy::default(),
            recording: false,
            source_controls: Default::default(),
        }
    }

    pub fn with_policy(inner: B, policy: crate::AudioContinuityPolicy) -> Self {
        Self {
            inner,
            policy,
            recording: true,
            source_controls: Default::default(),
        }
    }

    /// The wrapped backend, for a caller that still needs its own diagnostics.
    pub fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: AudioBackend> AudioBackend for NormalizingBackend<B> {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        self.inner.enumerate_audio_sources()
    }
}

impl<B: AudioCaptureBackend> AudioCaptureBackend for NormalizingBackend<B> {
    type Session = crate::RecoveringAudioSession<B::Session>;

    fn validate_capture_routes(
        &mut self,
        routes: &[frame_contracts::AudioRouteSelection],
    ) -> Result<(), RecorderError> {
        self.inner.validate_capture_routes(routes)?;
        self.source_controls = routes
            .iter()
            .map(|r| (r.source_id.clone(), r.controls))
            .collect();
        Ok(())
    }

    /// Starts exactly the requested source through the wrapped backend and normalizes
    /// its PCM. A failed start is returned unchanged, and no session is invented.
    fn start_audio(
        &mut self,
        source: &AudioSourceId,
        context: AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        let mut policy = self.policy;
        match self
            .source_controls
            .get(source)
            .map(|c| c.reconnect)
            .unwrap_or_default()
        {
            frame_contracts::AudioReconnectPolicy::ProcessDefault => {}
            frame_contracts::AudioReconnectPolicy::Silence => policy.reconnect_source = false,
            frame_contracts::AudioReconnectPolicy::ExactDevice => policy.reconnect_source = true,
        }
        let reopen = self.inner.reopen_audio_factory();
        let shared_clock = std::sync::Arc::clone(&context.clock);
        let cancellation = context.cancellation.clone();
        let session = self.inner.start_audio(
            source,
            AudioContext {
                clock: std::sync::Arc::clone(&shared_clock),
                cancellation: cancellation.child(),
            },
        )?;
        let session = crate::RecoveringAudioSession::new(
            session,
            source.clone(),
            shared_clock,
            cancellation,
            policy,
            reopen,
        );
        Ok(if self.recording {
            session.for_recording()
        } else {
            session
        })
    }
}
