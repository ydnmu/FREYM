//! Adapter-scoped Media Foundation preparation and one synthetic GPU-frame proof.
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    use frame_mux::ContainerWriter;

    let config = frame_windows::NativeEncoderConfig::qualification()?;
    for adapter in frame_windows::enumerate_native_adapters()? {
        if adapter.vendor_id != 0x8086 && adapter.vendor_id != 0x1002 {
            continue;
        }
        println!(
            "{} vendor={:04X} luid={:016X}",
            adapter.description, adapter.vendor_id, adapter.luid
        );
        for preparation in frame_windows::prepare_hardware_h264(&adapter, &config)? {
            println!(
                "  {} media_types_accepted={} {}",
                preparation.name, preparation.media_types_accepted, preparation.detail
            );
        }
        let candidates = frame_windows::probe_h264_encoder_for_adapter(
            &adapter,
            (config.width, config.height),
            (config.rate_num, config.rate_den),
        )?;
        if candidates.len() != 1 || candidates[0].adapter_luid != adapter.luid {
            return Err(format!(
                "Expected one qualified encoder on LUID {:016X}, found {}",
                adapter.luid,
                candidates.len()
            )
            .into());
        }
        println!("  qualified_adapter_candidates={}", candidates.len());
        let (info, packet) =
            frame_windows::synthetic_media_foundation_h264_frame(&adapter, &config)?;
        let path = std::env::temp_dir().join(format!("frame-mf-h264-{}.mkv", std::process::id()));
        let mut writer =
            frame_mux::MkvVideoWriter::create(&path, &info, frame_clock::MediaTimestamp(0))?;
        writer.write(packet)?;
        let artifact = writer.finalize()?;
        println!(
            "  synthetic_h264_drain_qualified={} config_bytes={} mkv={}",
            info.capability.id,
            info.codec_config.len(),
            artifact.path
        );
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {}
