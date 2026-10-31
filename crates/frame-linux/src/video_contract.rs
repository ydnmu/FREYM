//! Device and frozen-stream checks independent of GStreamer and the host OS.
use std::path::PathBuf;

#[cfg(any(target_os = "linux", test))]
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};

/// Resolving a DRM node's sysfs `device` link identifies the physical adapter.
/// A render-node filename or a driver/vendor name alone does not identify it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrmDeviceIdentity {
    pub node: PathBuf,
    pub physical_device: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaBufBlocker {
    CaptureDeviceUnknown,
    EncoderDeviceUnknown,
    ImportDeviceUnknown,
    DeviceMismatch,
    ImportCapsUnsupported,
    ModifierUnsupported,
    MemoryTypeMismatch,
    NegotiatedStreamChanged,
    StartupFailed,
    StartupTimedOut,
    InvalidPlaneLayout,
    SynchronizationUnsupported,
    ImportFailed,
    SynchronizationFailed,
    BufferOwnership,
}

/// A physical-identity check, not evidence that an import was performed.
/// Unknown identity fails this check; the native path can instead prove
/// compatibility by importing the actual PRIME buffer on its shared VA
/// display. Driver names/modifier vendor bits never identify an adapter.
pub fn assess_dma_buf_import(
    capture: Option<&DrmDeviceIdentity>,
    encoder: Option<&DrmDeviceIdentity>,
    import_caps_supported: bool,
) -> Result<(), DmaBufBlocker> {
    let capture = capture.ok_or(DmaBufBlocker::CaptureDeviceUnknown)?;
    let encoder = encoder.ok_or(DmaBufBlocker::EncoderDeviceUnknown)?;
    if capture.physical_device.as_os_str().is_empty()
        || encoder.physical_device.as_os_str().is_empty()
        || capture.physical_device != encoder.physical_device
    {
        return Err(DmaBufBlocker::DeviceMismatch);
    }
    if !import_caps_supported {
        return Err(DmaBufBlocker::ImportCapsUnsupported);
    }
    Ok(())
}

/// GStreamer's DMA_DRM caps pair the fourcc and modifier in one string. Linear
/// formats omit the modifier; treating an omitted modifier as arbitrary tiled
/// storage would make an otherwise valid caps intersection unsafe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmaDrmFormat {
    pub fourcc: String,
    pub modifier: u64,
}

impl DmaDrmFormat {
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() > 32 {
            return None;
        }
        let (fourcc, suffix) = value
            .split_once(':')
            .map_or((value, None), |(f, m)| (f, Some(m)));
        let code = fourcc.strip_suffix("_BE").unwrap_or(fourcc);
        if code.len() != 4 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let modifier = match suffix {
            None => 0,
            Some(suffix) => {
                let hex = suffix.strip_prefix("0x")?;
                if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                let modifier = u64::from_str_radix(hex, 16).ok()?;
                // Linear must be represented by fourcc alone; INVALID is not a
                // negotiated memory layout and cannot be imported safely.
                if modifier == 0 || modifier == u64::MAX || modifier == (1_u64 << 56) - 1 {
                    return None;
                }
                modifier
            }
        };
        Some(Self {
            fourcc: fourcc.into(),
            modifier,
        })
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn shm_fallback_allowed(output_frames: u64, draining: bool) -> bool {
    output_frames == 0 && !draining
}

#[cfg(any(target_os = "linux", test))]
pub(crate) struct VideoContract {
    geometry: (u32, u32),
    last_pts: Option<u64>,
    codec_config: Vec<u8>,
}

#[cfg(any(target_os = "linux", test))]
impl VideoContract {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            geometry: (width, height),
            last_pts: None,
            codec_config: Vec::new(),
        }
    }

    pub(crate) fn packet(
        &mut self,
        geometry: (u32, u32),
        pts: u64,
        dts: Option<u64>,
        parameter_sets: Option<&[u8]>,
    ) -> Result<(), RecorderError> {
        let message = if geometry.0 == 0 || geometry.1 == 0 || geometry != self.geometry {
            Some("The VA-API output changed the fixed screen dimensions.")
        } else if self.last_pts.is_some_and(|last| pts <= last) || dts.is_some_and(|dts| dts != pts)
        {
            Some("The VA-API output timestamps did not advance or were reordered.")
        } else if parameter_sets.is_some_and(|sets| {
            sets.is_empty() || (!self.codec_config.is_empty() && self.codec_config != sets)
        }) {
            Some("The VA-API H.264 parameter sets changed within the current track.")
        } else {
            None
        };
        if let Some(message) = message {
            return Err(RecorderError {
                code: ErrorCode::EncoderUnavailable,
                message: message.into(),
                recovery: RecoveryStatus::NoRecording,
                action: "Stop and start a new recording to use a changed device or stream configuration.".into(),
            });
        }
        if self.codec_config.is_empty()
            && let Some(sets) = parameter_sets
        {
            self.codec_config = sets.to_vec();
        }
        self.last_pts = Some(pts);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(node: &str, physical: &str) -> DrmDeviceIdentity {
        DrmDeviceIdentity {
            node: node.into(),
            physical_device: physical.into(),
        }
    }

    #[test]
    fn dma_buf_requires_known_same_physical_adapter_and_supported_caps() {
        let a = device("/dev/dri/card0", "/sys/devices/pci0000:00/0000:00:02.0");
        let a_render = device(
            "/dev/dri/renderD128",
            "/sys/devices/pci0000:00/0000:00:02.0",
        );
        let b = device(
            "/dev/dri/renderD129",
            "/sys/devices/pci0000:00/0000:01:00.0",
        );
        assess_dma_buf_import(Some(&a), Some(&a_render), true).unwrap();
        assert_eq!(
            assess_dma_buf_import(None, Some(&a_render), true),
            Err(DmaBufBlocker::CaptureDeviceUnknown)
        );
        assert_eq!(
            assess_dma_buf_import(Some(&a), None, true),
            Err(DmaBufBlocker::EncoderDeviceUnknown)
        );
        assert_eq!(
            assess_dma_buf_import(Some(&a), Some(&b), true),
            Err(DmaBufBlocker::DeviceMismatch)
        );
        assert_eq!(
            assess_dma_buf_import(Some(&a), Some(&a_render), false),
            Err(DmaBufBlocker::ImportCapsUnsupported)
        );
    }

    #[test]
    fn frozen_track_rejects_mutation_before_accepting_packet() {
        let mut contract = VideoContract::new(1920, 1080);
        contract
            .packet((1920, 1080), 1, None, Some(&[1, 2]))
            .unwrap();
        assert!(contract.packet((1920, 1080), 1, None, None).is_err());
        assert!(contract.packet((1920, 1080), 2, Some(1), None).is_err());
        assert!(contract.packet((1280, 720), 2, None, None).is_err());
        assert!(
            contract
                .packet((1920, 1080), 2, None, Some(&[1, 3]))
                .is_err()
        );
        // A refused mutation did not move the original timestamp/configuration.
        contract
            .packet((1920, 1080), 2, Some(2), Some(&[1, 2]))
            .unwrap();
    }

    #[test]
    fn dma_drm_modifier_is_exact_and_never_inferred_from_vendor() {
        assert_eq!(DmaDrmFormat::parse("NV12").unwrap().modifier, 0);
        assert_eq!(
            DmaDrmFormat::parse("AR24:0x0100000000000002")
                .unwrap()
                .modifier,
            0x0100000000000002
        );
        assert!(DmaDrmFormat::parse("AR24_BE").is_some());
        for invalid in [
            "NV12:0x0",
            "NV12:0x0000000000000000",
            "NV12:0xffffffffffffffff",
            "NV12:0x00ffffffffffffff",
            "NV12:0x0100000000000002:other",
            "DMA_DRM",
            "NV12:0x010000000000000g",
        ] {
            assert!(DmaDrmFormat::parse(invalid).is_none(), "{invalid}");
        }
        let a = DmaDrmFormat::parse("AR24:0x0100000000000001").unwrap();
        let b = DmaDrmFormat::parse("AR24:0x0100000000000002").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn shm_fallback_cannot_discard_published_output_or_resume_drain() {
        assert!(shm_fallback_allowed(0, false));
        assert!(!shm_fallback_allowed(1, false));
        assert!(!shm_fallback_allowed(0, true));
    }
}
