//! Recording output settings shared by settings, hosts and the existing Opus pipeline.
use crate::RecorderError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioCodec {
    #[default]
    Opus,
    Aac,
    Pcm,
    Flac,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioChannels {
    /// Stereo Main and each source's native mono/stereo stem.
    #[default]
    Preserve,
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioQuality {
    Voice,
    Standard,
    High,
    Best,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioSettings {
    pub codec: AudioCodec,
    /// The recording output rate; native input is independently normalized.
    pub sample_rate_hz: u32,
    /// VBR target per independent track, not an aggregate recording budget.
    pub bitrate_bps: u32,
    #[serde(default)]
    pub channels: AudioChannels,
}
impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            codec: AudioCodec::Opus,
            sample_rate_hz: 48_000,
            bitrate_bps: 128_000,
            channels: AudioChannels::Preserve,
        }
    }
}
impl AudioSettings {
    pub const SUPPORTED_CODECS: &[AudioCodec] = &[AudioCodec::Opus];
    pub const SUPPORTED_SAMPLE_RATES: &[u32] = &[48_000];
    pub const SUPPORTED_CHANNELS: &[AudioChannels] = &[
        AudioChannels::Preserve,
        AudioChannels::Mono,
        AudioChannels::Stereo,
    ];
    pub const BITRATE_RANGE: std::ops::RangeInclusive<u32> = 6_000..=510_000;

    pub fn validate(&self) -> Result<(), RecorderError> {
        if !Self::SUPPORTED_CODECS.contains(&self.codec) {
            return Err(RecorderError::unsupported(
                "Recording audio codec; this pipeline supports Opus",
            ));
        }
        if !Self::SUPPORTED_SAMPLE_RATES.contains(&self.sample_rate_hz) {
            return Err(RecorderError::unsupported(
                "Recording audio sample rate; Opus output uses 48 kHz (44.1/48/96 kHz capture is normalized)",
            ));
        }
        if !Self::BITRATE_RANGE.contains(&self.bitrate_bps) {
            return Err(RecorderError::invalid(
                "Opus bitrate must be 6,000…510,000 bit/s per track.",
            ));
        }
        Ok(())
    }

    /// Presets only change audio bitrate. Custom keeps the explicit bitrate.
    pub fn set_quality(&mut self, quality: AudioQuality) {
        self.bitrate_bps = match quality {
            AudioQuality::Voice => 64_000,
            AudioQuality::Standard => 128_000,
            AudioQuality::High => 192_000,
            AudioQuality::Best => 256_000,
            AudioQuality::Custom => self.bitrate_bps,
        };
    }
    pub fn quality(&self) -> AudioQuality {
        match self.bitrate_bps {
            64_000 => AudioQuality::Voice,
            128_000 => AudioQuality::Standard,
            192_000 => AudioQuality::High,
            256_000 => AudioQuality::Best,
            _ => AudioQuality::Custom,
        }
    }
    pub fn track_channels(&self, native: std::num::NonZeroU16) -> std::num::NonZeroU16 {
        match self.channels {
            AudioChannels::Preserve => native,
            AudioChannels::Mono => std::num::NonZeroU16::new(1).unwrap(),
            AudioChannels::Stereo => std::num::NonZeroU16::new(2).unwrap(),
        }
    }
}
