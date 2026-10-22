// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Device PCM formats, validated before any pointer is read (FRAME-0105, ADR 0007).
//!
//! Everything here is pure: it turns a validated description of the device mix format
//! into the exact interleaved `f32` layout the canonical [`frame_audio::PcmBlock`]
//! carries, and it decides every bound before a native buffer is touched. No COM type,
//! pointer or device is involved, so the conversion rules are covered by ordinary unit
//! tests on any Windows host.
//!
//! ADR 0007 scope: mono/stereo, 8_000..=192_000 Hz, IEEE float32 and signed PCM
//! 16/24/32-bit, including well-formed extensible layouts with valid-bit alignment. The
//! sample rate, the channel order and the frame count are preserved exactly: there is no
//! resampler, mixer, gain, clipping or downmix anywhere in this path.

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use std::num::{NonZeroU16, NonZeroU32};

/// The device's sample representation, as reported by the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleEncoding {
    /// IEEE 754 single precision (32-bit container).
    Float32,
    /// Signed PCM in a 16-, 24- or 32-bit container.
    Pcm {
        container_bits: u16,
        valid_bits: u16,
    },
}

impl SampleEncoding {
    /// The honest text rendering used by diagnostics.
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::Float32 => "ieee-float32",
            Self::Pcm {
                container_bits: 16, ..
            } => "pcm-s16",
            Self::Pcm {
                container_bits: 24, ..
            } => "pcm-s24",
            Self::Pcm {
                container_bits: 32, ..
            } => "pcm-s32",
            Self::Pcm { .. } => "pcm-unsupported-container",
        }
    }
}

/// A validated device mix format.
///
/// Constructing one proves every field a conversion depends on: channel count, rate,
/// container size, valid bits, block alignment and (for extensible layouts) the
/// subformat. Reading it is therefore safe for any buffer whose size is a whole number
/// of frames at this alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceFormat {
    sample_rate: NonZeroU32,
    channels: NonZeroU16,
    encoding: SampleEncoding,
    /// Bytes per whole frame, as the device declared it.
    block_align: u32,
    /// The device's own buffer capacity, in whole frames.
    buffer_frames: Option<u32>,
    // NOTE: `None` means no native capacity has been adopted yet; a declared layout
    // admits no packet until `with_buffer_frames` adopts the real GetBufferSize.
}

/// ADR 0007's proof scope, in one place.
pub(crate) const MIN_SAMPLE_RATE: u32 = 8_000;
pub(crate) const MAX_SAMPLE_RATE: u32 = 192_000;
pub(crate) const MAX_CHANNELS: u16 = 2;
/// The largest sample block this proof ever carries: 100 ms of device frames.
pub(crate) fn packet_frame_cap(sample_rate: NonZeroU32) -> u32 {
    // ceil(rate / 10) without floating point or overflow at the accepted maximum.
    sample_rate.get().saturating_add(9) / 10
}

fn unsupported(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message,
        recovery: RecoveryStatus::NoRecording,
        action:
            "Select a device whose mix format is mono or stereo 16/24/32-bit PCM or IEEE float32."
                .into(),
    }
}

fn malformed(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "The device reported a malformed mix format; this is a driver or OS defect, not a supported configuration."
            .into(),
    }
}

/// Private OS layout fields, validated before packet conversion.
#[derive(Clone, Copy)]
pub(crate) struct FormatDescription {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) bits_per_sample: u16,
    pub(crate) block_align: u16,
    pub(crate) valid_bits: u16,
    pub(crate) is_float: Option<bool>,
    pub(crate) extensible: bool,
}

impl DeviceFormat {
    #[cfg(test)]
    pub(crate) fn from_parts(
        description: FormatDescription,
        buffer_frames: u32,
    ) -> Result<Self, RecorderError> {
        Self::from_layout(description)?.with_buffer_frames(buffer_frames)
    }

    /// A layout alone admits no packet until the OS buffer capacity is adopted.
    pub(crate) fn from_layout(description: FormatDescription) -> Result<Self, RecorderError> {
        let FormatDescription {
            sample_rate,
            channels,
            bits_per_sample,
            block_align,
            valid_bits,
            is_float,
            extensible,
        } = description;
        let sample_rate = NonZeroU32::new(sample_rate)
            .ok_or_else(|| malformed("The device reported a zero sample rate.".to_string()))?;
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate.get()) {
            return Err(unsupported(format!(
                "A {} Hz device mix format is outside the supported {}..={} Hz range.",
                sample_rate, MIN_SAMPLE_RATE, MAX_SAMPLE_RATE
            )));
        }
        let channels = NonZeroU16::new(channels)
            .ok_or_else(|| malformed("The device reported zero channels.".to_string()))?;
        if channels.get() > MAX_CHANNELS {
            return Err(unsupported(format!(
                "A {}-channel device mix format is outside the supported mono/stereo scope.",
                channels
            )));
        }
        let is_float = is_float.ok_or_else(|| {
            malformed(format!(
                "The device reported a {bits_per_sample}-bit mix format without a recognised encoding subformat."
            ))
        })?;
        let encoding = match (is_float, bits_per_sample) {
            (true, 32) => {
                // An extensible float layout may declare the container's valid bits or
                // none; any other value would misdescribe the samples.
                if extensible && valid_bits != 0 && valid_bits != 32 {
                    return Err(malformed(format!(
                        "The device reported {valid_bits} valid bits for a 32-bit IEEE float container."
                    )));
                }
                SampleEncoding::Float32
            }
            (false, container @ (16 | 24 | 32)) => {
                let valid = if extensible { valid_bits } else { container };
                if valid == 0 || valid > container {
                    return Err(malformed(format!(
                        "The device reported {valid} valid bits in a {container}-bit PCM container."
                    )));
                }
                SampleEncoding::Pcm {
                    container_bits: container,
                    valid_bits: valid,
                }
            }
            (true, other) => {
                return Err(unsupported(format!(
                    "A {other}-bit floating-point device mix format is not supported."
                )));
            }
            (false, other) => {
                return Err(unsupported(format!(
                    "A {other}-bit PCM device mix format is not supported; 16, 24 and 32 bits are."
                )));
            }
        };

        // The declared alignment must be exactly the frame size this conversion reads, or
        // every pointer read below would be misaligned.
        let container_bytes = u32::from(bits_per_sample / 8);
        if !bits_per_sample.is_multiple_of(8) || container_bytes == 0 {
            return Err(malformed(format!(
                "The device reported {bits_per_sample} bits per sample, which is not a whole number of bytes."
            )));
        }
        let expected_align = container_bytes
            .checked_mul(u32::from(channels.get()))
            .ok_or_else(|| malformed("The frame alignment overflows.".to_string()))?;
        if u32::from(block_align) != expected_align {
            return Err(malformed(format!(
                "The device reported a block alignment of {block_align} bytes, but {} channels at {bits_per_sample} bits are {expected_align} bytes per frame.",
                channels
            )));
        }
        Ok(Self {
            sample_rate,
            channels,
            encoding,
            block_align: expected_align,
            buffer_frames: None,
        })
    }
    /// Validates and adopts the device's real `GetBufferSize` capacity.
    ///
    /// This is what makes ordinary packets legal: the declared layout alone is not a
    /// capacity, and a capacity beyond the proof's 100 ms frame bound is refused.
    pub(crate) fn with_buffer_frames(&self, buffer_frames: u32) -> Result<Self, RecorderError> {
        if buffer_frames == 0 {
            return Err(malformed(
                "The device reported a zero-length capture buffer.".to_string(),
            ));
        }
        let cap = packet_frame_cap(self.sample_rate);
        if buffer_frames > cap {
            return Err(malformed(format!(
                "The device reported a {buffer_frames}-frame capture buffer, above the {cap} frame (100 ms) packet bound."
            )));
        }
        Ok(Self {
            buffer_frames: Some(buffer_frames),
            ..*self
        })
    }

    pub(crate) fn sample_rate(&self) -> NonZeroU32 {
        self.sample_rate
    }

    pub(crate) fn channels(&self) -> NonZeroU16 {
        self.channels
    }

    pub(crate) fn encoding(&self) -> SampleEncoding {
        self.encoding
    }

    pub(crate) fn block_align(&self) -> u32 {
        self.block_align
    }

    pub(crate) fn buffer_frames(&self) -> u32 {
        self.buffer_frames.unwrap_or(0)
    }

    /// Bytes covered by `frames`, refusing anything above the packet bound.
    pub(crate) fn bytes_for_frames(&self, frames: u32) -> Result<usize, RecorderError> {
        let capacity = self.buffer_frames.ok_or_else(|| {
            malformed("No native buffer capacity has been adopted for this format.".into())
        })?;
        if frames > capacity {
            return Err(malformed(format!(
                "A {frames}-frame packet exceeds the native {capacity}-frame buffer."
            )));
        }
        if frames > packet_frame_cap(self.sample_rate) {
            return Err(malformed(format!(
                "A {frames}-frame native packet exceeds the {} frame bound.",
                packet_frame_cap(self.sample_rate)
            )));
        }
        let bytes = u64::from(frames)
            .checked_mul(u64::from(self.block_align))
            .ok_or_else(|| malformed("The native packet size overflows.".to_string()))?;
        usize::try_from(bytes)
            .map_err(|_| malformed("The native packet size is not addressable.".to_string()))
    }

    /// Interleaved `f32` samples covered by `frames`, refusing overflow.
    pub(crate) fn samples_for_frames(&self, frames: u32) -> Result<usize, RecorderError> {
        let samples = u64::from(frames)
            .checked_mul(u64::from(self.channels.get()))
            .ok_or_else(|| malformed("The sample count overflows.".to_string()))?;
        usize::try_from(samples)
            .map_err(|_| malformed("The sample count is not addressable.".to_string()))
    }

    /// Converts one whole native packet into interleaved `f32` without changing the rate,
    /// the channel order or the frame count.
    ///
    /// `data` must be the exact native buffer the OS reported for `frames`; a silent
    /// packet passes `None`, which produces the declared number of zero samples without
    /// reading any pointer.
    pub(crate) fn convert_into(
        &self,
        data: Option<&[u8]>,
        frames: u32,
        out: &mut Vec<f32>,
    ) -> Result<(), RecorderError> {
        // Every bound is checked before `out` is touched, so an oversized or malformed
        // packet is refused without allocating or retaining anything.
        let samples = self.samples_for_frames(frames)?;
        let expected = self.bytes_for_frames(frames)?;
        if let Some(bytes) = data
            && bytes.len() != expected
        {
            return Err(malformed(format!(
                "A native packet of {frames} frames must be {expected} bytes, but {} were supplied.",
                bytes.len()
            )));
        }
        let requested_bytes = (samples as u64) * size_of::<f32>() as u64;
        if requested_bytes > super::MAX_PCM_BLOCK_BYTES
            || (out.capacity() as u64) * size_of::<f32>() as u64 > super::MAX_PCM_BLOCK_BYTES
        {
            return Err(malformed(
                "PCM conversion exceeds its per-block capacity budget.".into(),
            ));
        }
        if out.capacity() < samples {
            // Release old scratch before reserving: at most one conversion allocation
            // coexists with the 32 queued blocks, including when packet sizes change.
            *out = Vec::new();
            out.try_reserve_exact(samples)
                .map_err(|_| malformed("PCM allocation failed.".into()))?;
            if (out.capacity() as u64) * size_of::<f32>() as u64 > super::MAX_PCM_BLOCK_BYTES {
                *out = Vec::new();
                return Err(malformed(
                    "PCM allocator capacity exceeds the block budget.".into(),
                ));
            }
        }
        let Some(bytes) = data else {
            out.clear();
            out.resize(samples, 0.0);
            return Ok(());
        };
        out.clear();
        match self.encoding {
            SampleEncoding::Float32 => {
                for chunk in bytes.chunks_exact(4) {
                    let raw = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    push_finite(out, raw)?;
                }
            }
            SampleEncoding::Pcm {
                container_bits,
                valid_bits,
            } => {
                let container = usize::from(container_bits / 8);
                for chunk in bytes.chunks_exact(container) {
                    let raw = match container_bits {
                        16 => i32::from(i16::from_le_bytes([chunk[0], chunk[1]])),
                        24 => {
                            let extended = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]);
                            // Sign-extend the 24-bit container.
                            (extended << 8) >> 8
                        }
                        32 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                        other => {
                            return Err(malformed(format!(
                                "A {other}-bit PCM container reached the converter."
                            )));
                        }
                    };
                    // Valid bits may be fewer than the container's. The padding is
                    // discarded and the value is scaled by the *valid* full-scale
                    // magnitude, so 24 valid bits in a 32-bit container reach the same
                    // normalized range as packed 24-bit PCM.
                    let shift = u32::from(container_bits - valid_bits);
                    let normalized = if shift == 0 { raw } else { raw >> shift };
                    let scale = (1i64 << (u32::from(valid_bits) - 1)) as f32;
                    push_finite(out, normalized as f32 / scale)?;
                }
            }
        }
        if out.len() != samples {
            return Err(malformed(format!(
                "Converting {frames} frames produced {} samples instead of {samples}.",
                out.len()
            )));
        }
        Ok(())
    }
}

fn push_finite(out: &mut Vec<f32>, value: f32) -> Result<(), RecorderError> {
    if !value.is_finite() {
        return Err(malformed(
            "The device produced a non-finite sample; the packet is refused instead of repaired."
                .to_string(),
        ));
    }
    out.push(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_clock::sample_timestamp;

    #[test]
    fn review_unadopted_native_capacity_cannot_convert_or_size_a_packet() {
        let declared = DeviceFormat::from_layout(FormatDescription {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 32,
            block_align: 8,
            valid_bits: 32,
            is_float: Some(true),
            extensible: false,
        })
        .expect("valid layout");
        let mut output = vec![0.25];
        assert!(declared.bytes_for_frames(1).is_err());
        assert!(declared.convert_into(None, 1, &mut output).is_err());
        assert_eq!(output, vec![0.25]);
    }

    #[test]
    fn review_conversion_checks_actual_capacity_before_touching_output() {
        let format = stereo_float().with_buffer_frames(480).unwrap();
        let mut output = vec![0.25];
        for data in [None, Some(&[][..])] {
            assert!(format.convert_into(data, 481, &mut output).is_err());
            assert_eq!(output, vec![0.25]);
        }
        format.convert_into(None, 480, &mut output).unwrap();
        assert_eq!(output.len(), 960);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    fn stereo_float() -> DeviceFormat {
        DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 32,
                block_align: 8,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            4800,
        )
        .unwrap()
    }

    #[test]
    fn packet_cap_is_one_hundred_milliseconds_and_never_rounds_down() {
        assert_eq!(packet_frame_cap(NonZeroU32::new(48_000).unwrap()), 4800);
        assert_eq!(packet_frame_cap(NonZeroU32::new(8_000).unwrap()), 800);
        assert_eq!(packet_frame_cap(NonZeroU32::new(192_000).unwrap()), 19_200);
        assert_eq!(packet_frame_cap(NonZeroU32::new(44_101).unwrap()), 4411);
    }

    #[test]
    fn float_and_integer_formats_convert_to_the_same_normalized_range() {
        let float = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 32,
                block_align: 4,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            480,
        )
        .unwrap();
        let mut out = Vec::new();
        float
            .convert_into(Some(&1.0f32.to_le_bytes()), 1, &mut out)
            .unwrap();
        assert_eq!(out, vec![1.0]);
        float
            .convert_into(Some(&(-1.0f32).to_le_bytes()), 1, &mut out)
            .unwrap();
        assert_eq!(out, vec![-1.0]);

        let pcm16 = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 16,
                block_align: 2,
                valid_bits: 16,
                is_float: Some(false),
                extensible: false,
            },
            480,
        )
        .unwrap();
        pcm16
            .convert_into(Some(&i16::MAX.to_le_bytes()), 1, &mut out)
            .unwrap();
        assert!((out[0] - 1.0).abs() < 0.0001);
        pcm16
            .convert_into(Some(&i16::MIN.to_le_bytes()), 1, &mut out)
            .unwrap();
        assert_eq!(out[0], -1.0);
    }

    #[test]
    fn packed_24_bit_pcm_is_sign_extended_and_scaled() {
        let pcm24 = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 24,
                block_align: 3,
                valid_bits: 24,
                is_float: Some(false),
                extensible: false,
            },
            480,
        )
        .unwrap();
        let mut out = Vec::new();
        pcm24
            .convert_into(Some(&[0x00, 0x00, 0x80]), 1, &mut out)
            .unwrap();
        assert_eq!(out[0], -1.0, "0x800000 must be full-scale negative");
        pcm24
            .convert_into(Some(&[0xff, 0xff, 0x7f]), 1, &mut out)
            .unwrap();
        assert!(out[0] > 0.999);
    }

    #[test]
    fn fewer_valid_bits_than_container_bits_are_honoured() {
        // 24 valid bits inside a 32-bit container: the low 8 bits are padding.
        let pcm32 = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 32,
                block_align: 4,
                valid_bits: 24,
                is_float: Some(false),
                extensible: true,
            },
            480,
        )
        .unwrap();
        let mut out = Vec::new();
        let full_scale: i32 = 0x7FFF_FF00u32 as i32;
        pcm32
            .convert_into(Some(&full_scale.to_le_bytes()), 1, &mut out)
            .unwrap();
        assert!(out[0] > 0.99 && out[0] <= 1.0, "got {}", out[0]);
    }

    #[test]
    fn stereo_conversion_preserves_channel_order_and_frame_count() {
        let format = stereo_float();
        let mut bytes = Vec::new();
        for frame in 0..3 {
            bytes.extend_from_slice(&(frame as f32).to_le_bytes());
            bytes.extend_from_slice(&(-(frame as f32)).to_le_bytes());
        }
        let mut out = Vec::new();
        format.convert_into(Some(&bytes), 3, &mut out).unwrap();
        assert_eq!(out, vec![0.0, 0.0, 1.0, -1.0, 2.0, -2.0]);
    }

    #[test]
    fn a_silent_packet_needs_no_data_pointer() {
        let format = stereo_float();
        let mut out = vec![9.0];
        format.convert_into(None, 2, &mut out).unwrap();
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn malformed_native_metadata_is_refused() {
        // Zero rate, zero channels, misaligned block, oversized buffer, bad valid bits.
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 0,
                    channels: 2,
                    bits_per_sample: 32,
                    block_align: 8,
                    valid_bits: 32,
                    is_float: Some(true),
                    extensible: false
                },
                480
            )
            .is_err()
        );
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 48_000,
                    channels: 0,
                    bits_per_sample: 32,
                    block_align: 4,
                    valid_bits: 32,
                    is_float: Some(true),
                    extensible: false
                },
                480
            )
            .is_err()
        );
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 48_000,
                    channels: 2,
                    bits_per_sample: 32,
                    block_align: 6,
                    valid_bits: 32,
                    is_float: Some(true),
                    extensible: false
                },
                480
            )
            .is_err()
        );
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 48_000,
                    channels: 2,
                    bits_per_sample: 32,
                    block_align: 8,
                    valid_bits: 32,
                    is_float: Some(true),
                    extensible: false
                },
                0
            )
            .is_err()
        );
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 48_000,
                    channels: 2,
                    bits_per_sample: 32,
                    block_align: 8,
                    valid_bits: 32,
                    is_float: Some(true),
                    extensible: false
                },
                4801
            )
            .is_err()
        );
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 48_000,
                    channels: 1,
                    bits_per_sample: 16,
                    block_align: 2,
                    valid_bits: 0,
                    is_float: Some(false),
                    extensible: true
                },
                480
            )
            .is_err()
        );
        assert!(
            DeviceFormat::from_parts(
                FormatDescription {
                    sample_rate: 48_000,
                    channels: 1,
                    bits_per_sample: 16,
                    block_align: 2,
                    valid_bits: 17,
                    is_float: Some(false),
                    extensible: true
                },
                480
            )
            .is_err()
        );
        // Unsupported but well-formed scopes are NotImplemented, not malformed.
        let wide = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 6,
                bits_per_sample: 32,
                block_align: 24,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            480,
        )
        .expect_err("six channels is outside the proof scope");
        assert_eq!(wide.code, ErrorCode::NotImplemented);
        let slow = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 4_000,
                channels: 2,
                bits_per_sample: 32,
                block_align: 8,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            400,
        )
        .expect_err("4000 Hz is outside the proof scope");
        assert_eq!(slow.code, ErrorCode::NotImplemented);
        let odd = DeviceFormat::from_parts(
            FormatDescription {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 12,
                block_align: 8,
                valid_bits: 12,
                is_float: Some(false),
                extensible: false,
            },
            480,
        )
        .expect_err("12-bit PCM is not in the proof scope");
        assert_eq!(odd.code, ErrorCode::NotImplemented);
    }

    #[test]
    fn a_native_packet_must_match_its_declared_frame_count_exactly() {
        let format = stereo_float();
        let mut out = Vec::new();
        assert!(format.convert_into(Some(&[0u8; 7]), 1, &mut out).is_err());
        assert!(format.convert_into(Some(&[0u8; 9]), 1, &mut out).is_err());
        // Above the 100 ms frame bound is refused before allocating.
        assert!(
            format
                .samples_for_frames(packet_frame_cap(format.sample_rate()) + 1)
                .is_err()
                || format
                    .bytes_for_frames(packet_frame_cap(format.sample_rate()) + 1)
                    .is_err()
        );
    }

    #[test]
    fn nonfinite_float_samples_are_refused_not_repaired() {
        let format = stereo_float();
        let mut out = Vec::new();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                format
                    .convert_into(Some(&bad.to_le_bytes()), 1, &mut out)
                    .is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn block_durations_use_the_exact_sample_conversion() {
        let rate = NonZeroU32::new(48_000).unwrap();
        assert_eq!(sample_timestamp(480, rate).unwrap().0, 10_000_000);
        let odd = NonZeroU32::new(44_100).unwrap();
        assert_eq!(sample_timestamp(441, odd).unwrap().0, 10_000_000);
    }
}
