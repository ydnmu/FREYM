//! One actual same-adapter alternative qualification. No recording or default change.
#[cfg(not(windows))]
fn main() {}
#[cfg(windows)]
fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|v| v == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use frame_capture::DisplayBackend;
    use frame_windows::{NativeEncoderConfig, PlatformBackend, WindowsH264Encoder};
    use std::{io::Write, num::NonZeroU32, path::PathBuf};
    let output = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("absolute new JSON output required")?,
    );
    if !output.is_absolute() {
        return Err("absolute JSON output required".into());
    }
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut selected = None;
    for display in PlatformBackend.enumerate_displays()? {
        let adapter = frame_windows::display_capture_adapter_identity(&display.id)?;
        if adapter.vendor_id == 0x10de {
            selected = Some((display, adapter));
            break;
        }
    }
    let (display, adapter) = selected.ok_or("no attached NVIDIA capture adapter")?;
    let config = NativeEncoderConfig::proof(
        display.width,
        display.height,
        NonZeroU32::new(60).unwrap(),
        NonZeroU32::new(1).unwrap(),
    )?;
    let result = WindowsH264Encoder::qualify_segment_alternative(config, &adapter);
    let report = serde_json::json!({
        "adapter_luid": adapter.luid, "adapter": adapter.description,
        "width": display.width, "height": display.height, "requested_fps": 60,
        "qualified": result.is_ok(),
        "selected_backend": result.as_ref().ok().map(|v| &v.capability.id),
        "error": result.as_ref().err().map(|v| &v.message),
        "native_failure_transition_acceptance": false,
        "cpu_transport_default_changed": false
    });
    target.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;
    target.sync_all()?;
    println!("{report}");
    Ok(())
}
