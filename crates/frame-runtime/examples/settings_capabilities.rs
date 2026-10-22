//! Capture-free local evidence. Real encoder qualification may create helper processes.
use frame_contracts::*;
use frame_settings::model::RecordingConfiguration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        // Native MF qualification uses the same isolated child protocol as the app.
        if std::env::args().any(|a| a == "--frame-mf-helper") {
            std::process::exit(frame_windows::run_mf_helper());
        }
    }
    let snapshot = frame_runtime::discovery::snapshot()?;
    let display = snapshot
        .displays
        .iter()
        .find(|d| d.is_primary)
        .or_else(|| snapshot.displays.first())
        .ok_or("No display available")?;
    let c: RecordingConfiguration = RecordingRequest {
        target: CaptureTarget::Display(display.id.clone()),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: Some(RecordingOptimization::default()),
        audio_routes: vec![],
        camera: None,
        show_cursor: true,
        output: OutputSettings {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
            file_stem: "capability-probe".into(),
        },
    }
    .into();
    let caps = frame_runtime::settings::discover(&c, &RecorderState::Idle)?;
    println!("OS: {:?}, build: {:?}", caps.os, caps.os_build);
    for gpu in &caps.gpus {
        println!(
            "GPU: {} {} vendor={:04X} driver={:?}",
            gpu.id, gpu.name, gpu.vendor_id, gpu.driver_version
        );
    }
    if let Some(video) = &caps.video {
        println!(
            "Capture: {}x{}, HDR={}, GPU={:?}; configuration rate={:?}",
            video.native.width,
            video.native.height,
            video.hdr_source,
            video.capture_gpu_id,
            video.nominal_rate
        );
        for encoder in &video.candidates {
            println!(
                "Encoder: {} codec={} hardware={} limits={:?}",
                encoder.id, encoder.codec, encoder.hardware, encoder.limits
            );
        }
    }
    for (source, qualification) in &caps.audio_devices {
        println!("Audio device: {} {:?}", source.0, qualification);
    }
    for (camera, formats) in &caps.cameras {
        println!(
            "Camera: {} {} native formats={}",
            camera.id,
            camera.name,
            formats.len()
        );
    }
    for diagnostic in &caps.diagnostics {
        println!("Diagnostic: {diagnostic}");
    }
    let report = frame_settings::validation::validate_configuration(&c, &caps);
    println!("Settings qualified: {}", report.is_valid());
    for issue in report.issues {
        println!("Issue: {} {:?}: {}", issue.field, issue.code, issue.message);
    }
    Ok(())
}
