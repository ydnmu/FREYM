//! Explicit same-adapter replacement. No startup/default discovery changes.
use super::*;

impl WindowsH264Encoder {
    /// Diagnostic qualification, not automatic discovery or product admission.
    /// A completed GPU frame and EOS at this exact configuration are required.
    pub fn qualify_segment_alternative(
        config: NativeEncoderConfig,
        adapter: &NativeAdapterIdentity,
    ) -> Result<NativeEncoderCandidate, RecorderError> {
        if adapter.vendor_id != 0x10de {
            return Err(preparation_error(
                "No alternative backend is declared for this adapter.".into(),
            ));
        }
        let settings = mf_process::qualify_adapter(adapter, &config)?;
        Ok(NativeEncoderCandidate {
            capability: EncoderCapability {
                id: format!("nvidia-mf-h264-d3d11:luid={:016X}", adapter.luid),
                codec: "h264".into(),
                hardware: true,
            },
            adapter: adapter.description.clone(),
            adapter_luid: adapter.luid,
            settings,
        })
    }
    /// Qualifies an alternative at the exact frozen configuration before opening
    /// it on the actual captured device. Failed qualification keeps the original
    /// route and returns its reason; a failed qualified open remains an error.
    pub fn open_segment_replacement(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
        previous_id: &str,
        allow_alternative: bool,
    ) -> Result<(Self, Option<String>), RecorderError> {
        let adapter = capture_adapter_identity(lease)?;
        if alternative_is_eligible(previous_id, adapter.luid, lease.format(), allow_alternative) {
            match Self::qualify_segment_alternative(config, &adapter) {
                Ok(_) => {
                    let encoder = Self::open_isolated_mf(config, lease)?;
                    return Ok((encoder, None));
                }
                Err(error) => {
                    let encoder = Self::open_selected(config, lease, previous_id)?;
                    return Ok((
                        encoder,
                        Some(format!(
                            "Same-adapter MF qualification rejected: {}",
                            error.message
                        )),
                    ));
                }
            }
        }
        Self::open_selected(config, lease, previous_id).map(|encoder| (encoder, None))
    }

    /// This isolated route is reachable only by an explicit selected ID or the
    /// opted-in segment replacement above. MFTEnum2 scopes hardware activations
    /// to this LUID; the helper retains its existing CPU transport default.
    pub(super) fn open_isolated_mf(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        if lease.format() != SurfaceFormat::Bgra8 {
            return Err(preparation_error(
                "Alternative MF accepts only BGRA8 SDR.".into(),
            ));
        }
        let texture = lease.encode_texture().ok_or_else(|| {
            preparation_error("Alternative MF requires the capture device texture.".into())
        })?;
        if texture.identity.width != config.width
            || texture.identity.height != config.height
            || texture.device.is_null()
            || texture.texture.is_null()
        {
            return Err(preparation_error(
                "Alternative MF capture geometry/device mismatch.".into(),
            ));
        }
        let adapter = native::device_adapter(texture.device)?;
        if adapter.vendor_id != 0x10de {
            return Err(preparation_error(
                "The alternative route requires the selected NVIDIA device.".into(),
            ));
        }
        let id = format!("nvidia-mf-h264-d3d11:luid={:016X}", adapter.luid);
        let device_address = texture.device as usize;
        let mut encoder = Self::start_with_prepared(
            config,
            texture.identity,
            lease.encode_backing(),
            lease.is_preparation(),
            move || {
                mf_process::MfProcessDriver::open(
                    device_address as *mut std::ffi::c_void,
                    &adapter,
                    &config,
                )
                .map(|driver| Box::new(driver) as Box<dyn NvencDriver>)
            },
        )?;
        encoder.info.capability.id = id;
        Ok(encoder)
    }
}

fn alternative_is_eligible(id: &str, luid: u64, format: SurfaceFormat, enabled: bool) -> bool {
    enabled
        && format == SurfaceFormat::Bgra8
        && id == format!("nvidia-nvenc-h264-d3d11:luid={luid:016X}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alternative_admission_is_exact_adapter_codec_surface_and_opt_in() {
        let id = "nvidia-nvenc-h264-d3d11:luid=0000000000000012";
        assert!(alternative_is_eligible(
            id,
            0x12,
            SurfaceFormat::Bgra8,
            true
        ));
        assert!(!alternative_is_eligible(
            id,
            0x13,
            SurfaceFormat::Bgra8,
            true
        ));
        assert!(!alternative_is_eligible(
            id,
            0x12,
            SurfaceFormat::ScRgbFloat16,
            true
        ));
        assert!(!alternative_is_eligible(
            id,
            0x12,
            SurfaceFormat::Bgra8,
            false
        ));
        assert!(!alternative_is_eligible(
            "nvidia-nvenc-hevc-main10-d3d11:luid=0000000000000012",
            0x12,
            SurfaceFormat::Bgra8,
            true
        ));
    }
    #[test]
    fn native_fault_flag_does_not_promote_an_owner_or_queue_error() {
        let shared = EncoderShared::new();
        shared.fail(internal_error("queue contract".into()));
        assert!(!shared.native_fault.load(Ordering::Acquire));
        let native = EncoderShared::new();
        let fault = native.native_error(internal_error("native collect".into()));
        assert!(native.terminal.get().is_none()); // No terminal result before cleanup.
        native.fail(fault);
        assert!(native.native_fault.load(Ordering::Acquire));
        assert!(native.terminal.get().is_some());
    }
}
