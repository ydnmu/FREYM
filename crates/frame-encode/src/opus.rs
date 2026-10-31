// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Synchronous 48 kHz mono/stereo Opus encoding for one aligned PCM track.
//!
//! One [`OpusTrackEncoder`] owns exactly one libopus encoder for exactly one
//! `TrackPcm` track. `TrackPcm` stays the only input and `EncodedPacket` the only
//! output, so the video encoder contract is untouched. Packet timestamps stay on
//! the encoded-sample axis *before* trimming; the OpusHead pre-skip and the final
//! packet's discard padding are reported explicitly by [`OpusTrackEncoder::finish`].
//!
//! There is no history, worker thread or unbounded queue: the PCM and compressed
//! packet buffers are fixed scratch and at most one Opus frame is ever pending.

use crate::EncodedPacket;
use frame_audio::TrackPcm;
use frame_clock::{MediaTimestamp, sample_timestamp};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use opus::{Application, Bitrate, Channels, Encoder};
use std::num::{NonZeroU16, NonZeroU32};

/// The only accepted input rate. No resampling or rate conversion happens here.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo default; `with_channels` also preserves interleaved mono f32.
pub const CHANNELS: u16 = 2;
/// Samples per channel in one 20 ms Opus frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
/// Largest accepted input block per call: one 10 ms mixer quantum.
pub const MAX_INPUT_SAMPLES: usize = 480;

const RATE: NonZeroU32 = NonZeroU32::new(SAMPLE_RATE).expect("48 kHz is nonzero");
const CHANNEL_COUNT: NonZeroU16 = NonZeroU16::new(CHANNELS).expect("stereo is nonzero");
const CHANNEL_USIZE: usize = CHANNELS as usize;
const SCRATCH_SAMPLES: usize = FRAME_SAMPLES * CHANNEL_USIZE;
/// Contractual ceiling for one Opus frame; a single Opus frame is at most 1275 bytes.
const MAX_PACKET_BYTES: usize = 4_000;

/// What the flush produced, plus the trim metadata an Opus header needs.
#[derive(Debug)]
pub struct OpusTrackFinish {
    /// Packets produced by the flush, in order, after every packet `submit` returned.
    pub packets: Vec<EncodedPacket>,
    /// OpusHead pre-skip in 48 kHz samples per channel: the encoder's real lookahead.
    pub pre_skip: u16,
    /// Samples to discard from the **end** of the last packet (48 kHz, per channel).
    pub discard_padding: u16,
    /// Total accepted input frames per channel.
    pub input_frames: u64,
}

/// One synchronous encoder for one track. Each track needs its own instance.
pub struct OpusTrackEncoder {
    channels: NonZeroU16,
    track_id: u32,
    track_index: usize,
    origin: MediaTimestamp,
    encoder: Encoder,
    pre_skip: u16,
    /// Fixed interleaved f32 scratch holding at most one incomplete Opus frame.
    pcm: [f32; SCRATCH_SAMPLES],
    /// Fixed compressed scratch, reused for every emitted packet.
    packet: [u8; MAX_PACKET_BYTES],
    /// Input frames currently held in `pcm`; always below `FRAME_SAMPLES`.
    buffered: usize,
    /// Input frames accepted so far; the only source of the packet time axis.
    input_frames: u64,
    /// Opus frames emitted so far.
    encoded_frames: u64,
    /// Sticky codec failure: accepted PCM is never silently dropped or mis-timed.
    sticky: Option<RecorderError>,
    finished: bool,
}

impl OpusTrackEncoder {
    /// `track_index` is the plan index a `TrackPcm` carries and becomes the packet
    /// `track_id`; `origin` is the recording's shared media origin. The encoder is
    /// configured once here: 48 kHz stereo f32, Audio application, 128 kbit/s, VBR
    /// on, DTX and FEC off.
    pub fn new(track_index: usize, origin: MediaTimestamp) -> Result<Self, RecorderError> {
        Self::with_channels(track_index, origin, CHANNEL_COUNT)
    }

    pub fn with_channels(
        track_index: usize,
        origin: MediaTimestamp,
        channels: NonZeroU16,
    ) -> Result<Self, RecorderError> {
        Self::with_settings(
            track_index,
            origin,
            channels,
            &frame_contracts::AudioSettings::default(),
        )
    }

    pub fn with_settings(
        track_index: usize,
        origin: MediaTimestamp,
        channels: NonZeroU16,
        settings: &frame_contracts::AudioSettings,
    ) -> Result<Self, RecorderError> {
        settings.validate()?;
        let opus_channels = match channels.get() {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => return Err(RecorderError::unsupported("Opus channel layout")),
        };
        let track_id = u32::try_from(track_index).map_err(|_| {
            RecorderError::invalid("The audio track index does not fit the packet track id.")
        })?;
        let mut encoder =
            Encoder::new(SAMPLE_RATE, opus_channels, Application::Audio).map_err(|error| {
                codec_error(
                    ErrorCode::EncoderUnavailable,
                    "create the Opus encoder",
                    error,
                )
            })?;
        encoder
            .set_bitrate(Bitrate::Bits(settings.bitrate_bps as i32))
            .map_err(|error| {
                codec_error(ErrorCode::EncoderUnavailable, "set the Opus bitrate", error)
            })?;
        encoder.set_vbr(true).map_err(|error| {
            codec_error(
                ErrorCode::EncoderUnavailable,
                "enable Opus variable bitrate",
                error,
            )
        })?;
        encoder.set_dtx(false).map_err(|error| {
            codec_error(
                ErrorCode::EncoderUnavailable,
                "disable Opus discontinuous transmission",
                error,
            )
        })?;
        encoder.set_inband_fec(false).map_err(|error| {
            codec_error(
                ErrorCode::EncoderUnavailable,
                "disable Opus inband FEC",
                error,
            )
        })?;
        // The real codec delay, read from the encoder instead of assumed.
        let lookahead = encoder.get_lookahead().map_err(|error| {
            codec_error(
                ErrorCode::EncoderUnavailable,
                "read the Opus lookahead",
                error,
            )
        })?;
        let pre_skip = u16::try_from(lookahead).map_err(|_| {
            RecorderError::invalid("The Opus lookahead does not fit the OpusHead pre-skip field.")
        })?;
        if usize::from(pre_skip) >= FRAME_SAMPLES {
            return Err(RecorderError::unsupported(
                "Opus lookahead of one packet or more exceeds the two-packet flush bound",
            ));
        }
        Ok(Self {
            channels,
            track_id,
            track_index,
            origin,
            encoder,
            pre_skip,
            pcm: [0.0; SCRATCH_SAMPLES],
            packet: [0u8; MAX_PACKET_BYTES],
            buffered: 0,
            input_frames: 0,
            encoded_frames: 0,
            sticky: None,
            finished: false,
        })
    }

    /// The plan index this encoder was built for, also used as the packet track id.
    pub fn track_id(&self) -> u32 {
        self.track_id
    }

    /// The encoder's real lookahead, before any input: the OpusHead pre-skip value.
    pub fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    /// Accepts one aligned block of this track. At most one packet is returned,
    /// because a block is at most half an Opus frame. A rejected block changes no
    /// state; a codec failure is sticky.
    pub fn submit(&mut self, pcm: &TrackPcm) -> Result<Option<EncodedPacket>, RecorderError> {
        self.check_usable()?;
        self.check_input(pcm)?;
        let frames = pcm.samples.len() / usize::from(self.channels.get());
        let space = FRAME_SAMPLES - self.buffered;
        if frames < space {
            self.append(&pcm.samples);
            self.input_frames += frames as u64;
            return Ok(None);
        }
        // A block is at most MAX_INPUT_SAMPLES < FRAME_SAMPLES, so completing the
        // pending frame leaves less than one frame behind: exactly one packet.
        let (head, tail) = pcm
            .samples
            .split_at(space * usize::from(self.channels.get()));
        self.append(head);
        self.input_frames += frames as u64;
        let packet = self.encode_frame()?;
        self.append(tail);
        Ok(Some(packet))
    }

    /// Flushes the pending partial frame and the remaining real codec lookahead,
    /// then closes admission. An empty track emits no packet. The trimmed decoded
    /// length equals the accepted input exactly: `pre_skip + discard_padding`
    /// accounts for every sample the codec adds.
    pub fn finish(&mut self) -> Result<OpusTrackFinish, RecorderError> {
        self.check_usable()?;
        if self.finished {
            return Err(RecorderError::invalid(
                "The Opus track encoder is already finished.",
            ));
        }
        let input_frames = self.input_frames;
        let pre_skip = u64::from(self.pre_skip);
        // Decoded output must cover the input plus the codec's own delay. With no
        // input at all the track stays packet-free instead of emitting silence.
        let total_frames = if input_frames == 0 {
            0
        } else {
            (input_frames + pre_skip).div_ceil(FRAME_SAMPLES as u64)
        };
        // At most two flush packets: the pending frame plus the lookahead remainder.
        let mut packets = Vec::new();
        while self.encoded_frames < total_frames {
            let start = self.buffered * usize::from(self.channels.get());
            self.pcm[start..].fill(0.0);
            self.buffered = FRAME_SAMPLES;
            packets.push(self.encode_frame()?);
        }
        let decoded = total_frames * FRAME_SAMPLES as u64;
        // Saturating only matters for the packet-free empty track, where nothing
        // was decoded and nothing has to be trimmed.
        let discard_padding = decoded.saturating_sub(pre_skip + input_frames);
        // Trim invariant: after the decoder discards pre-skip and the trailing
        // padding, exactly the accepted input remains. An empty track adds neither.
        debug_assert!(input_frames == 0 || decoded == pre_skip + discard_padding + input_frames);
        let discard_padding = u16::try_from(discard_padding).map_err(|_| {
            RecorderError::invalid("The Opus discard padding does not fit the flush report.")
        })?;
        self.finished = true;
        Ok(OpusTrackFinish {
            packets,
            pre_skip: self.pre_skip,
            discard_padding,
            input_frames,
        })
    }

    fn check_usable(&self) -> Result<(), RecorderError> {
        if let Some(error) = &self.sticky {
            return Err(error.clone());
        }
        Ok(())
    }

    /// Every mismatch is an explicit error, never a coercion or a route substitution.
    fn check_input(&self, pcm: &TrackPcm) -> Result<(), RecorderError> {
        if self.finished {
            return Err(RecorderError::invalid(
                "The Opus track encoder is finished and rejects further input.",
            ));
        }
        if pcm.track_index != self.track_index {
            return Err(RecorderError::invalid(
                "The PCM block belongs to a different audio track.",
            ));
        }
        if pcm.sample_rate != RATE {
            return Err(RecorderError::invalid(
                "Opus encoding accepts only 48 kHz PCM.",
            ));
        }
        if pcm.channels != self.channels {
            return Err(RecorderError::invalid(
                "PCM channel count differs from this Opus track's frozen layout.",
            ));
        }
        if !pcm
            .samples
            .len()
            .is_multiple_of(usize::from(self.channels.get()))
        {
            return Err(RecorderError::invalid(
                "Interleaved PCM must contain whole channel frames.",
            ));
        }
        let frames = pcm.samples.len() / usize::from(self.channels.get());
        if frames == 0 || frames > MAX_INPUT_SAMPLES {
            return Err(RecorderError::invalid(
                "A PCM block must contain 1 to 480 channel frames.",
            ));
        }
        if pcm.timestamp != self.timestamp_of_samples(self.input_frames)? {
            return Err(RecorderError::invalid(
                "The PCM block does not continue the encoded sample timeline.",
            ));
        }
        if pcm.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(RecorderError::invalid(
                "PCM samples must be finite f32 values.",
            ));
        }
        // Admission must leave enough timestamp range for the codec delay and the
        // padded final packet, before either PCM or libopus state is changed.
        let end = self
            .input_frames
            .checked_add(frames as u64)
            .and_then(|end| end.checked_add(u64::from(self.pre_skip)))
            .and_then(|end| {
                end.div_ceil(FRAME_SAMPLES as u64)
                    .checked_mul(FRAME_SAMPLES as u64)
            })
            .ok_or_else(|| RecorderError::invalid("The Opus sample timeline overflows."))?;
        self.timestamp_of_samples(end)?;
        Ok(())
    }

    fn append(&mut self, samples: &[f32]) {
        let start = self.buffered * usize::from(self.channels.get());
        self.pcm[start..start + samples.len()].copy_from_slice(samples);
        self.buffered += samples.len() / usize::from(self.channels.get());
    }

    /// Encodes the complete pending frame. The packet time axis is the count of
    /// Opus frames already emitted, i.e. the sample axis before any trimming.
    fn encode_frame(&mut self) -> Result<EncodedPacket, RecorderError> {
        debug_assert_eq!(self.buffered, FRAME_SAMPLES);
        let index = self.encoded_frames;
        let pts = self.timestamp_of_samples(index * FRAME_SAMPLES as u64)?;
        self.buffered = 0;
        let result = self.encoder.encode_float(
            &self.pcm[..FRAME_SAMPLES * usize::from(self.channels.get())],
            &mut self.packet,
        );
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(error) => {
                let error = codec_error(ErrorCode::Internal, "encode an Opus packet", error);
                self.sticky = Some(error.clone());
                return Err(error);
            }
        };
        let data = self.packet[..bytes].to_vec();
        self.encoded_frames = index + 1;
        Ok(EncodedPacket {
            track_id: self.track_id,
            pts,
            dts: pts,
            duration_ns: sample_timestamp(FRAME_SAMPLES as u64, RATE)
                .expect("960 samples at 48 kHz fits a media timestamp")
                .0,
            // Opus has no video-style keyframe distinction. A container still
            // needs Opus seek pre-roll; this flag does not remove that requirement.
            keyframe: true,
            data,
        })
    }

    /// The single time derivation: the shared origin plus an integer sample count.
    /// Input continuity and packet PTS both go through this one function.
    fn timestamp_of_samples(&self, samples: u64) -> Result<MediaTimestamp, RecorderError> {
        let offset = sample_timestamp(samples, RATE).ok_or_else(|| {
            RecorderError::invalid("The PCM timeline overflows the media timestamp range.")
        })?;
        self.origin
            .0
            .checked_add(offset.0)
            .map(MediaTimestamp)
            .ok_or_else(|| {
                RecorderError::invalid("The PCM timeline overflows the media timestamp range.")
            })
    }
}

fn codec_error(code: ErrorCode, action: &str, error: opus::Error) -> RecorderError {
    RecorderError {
        code,
        message: format!("Failed to {action}: {error}"),
        recovery: RecoveryStatus::NoRecording,
        action: "Review the audio encoder configuration and try again.".into(),
    }
}
