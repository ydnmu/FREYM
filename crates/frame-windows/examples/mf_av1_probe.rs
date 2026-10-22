//! Short adapter-scoped native AV1 qualification, including a real GPU picture/EOS.
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    let config = frame_windows::NativeEncoderConfig::qualification()?;
    for adapter in frame_windows::enumerate_native_adapters()? {
        if !matches!(adapter.vendor_id, 0x10de | 0x8086 | 0x1002) {
            continue;
        }
        match frame_windows::probe_mf_av1_for_adapter(&adapter, &config) {
            Ok(candidate) => println!(
                "{}: AV1 Main 8-bit GPU picture/EOS qualified; {}",
                candidate.capability.id,
                candidate.settings.codec_config.len()
            ),
            Err(error) => println!(
                "{} LUID {:016X}: unavailable: {}",
                adapter.description, adapter.luid, error.message
            ),
        }
    }
    Ok(())
}
#[cfg(not(target_os = "windows"))]
fn main() {}
