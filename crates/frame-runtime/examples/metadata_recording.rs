//! Essential Windows end-to-end check with explicit capture authorization.
//! cargo run -p frame-runtime --example metadata_recording --
//!   --allow-screen-capture C:/FREYM/build/<evidence>/new.mkv
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    use frame_capture::DisplayBackend;
    use frame_clock::MasterClock;
    use frame_runtime::video_recording::{
        VideoRecording, VideoRecordingPhase, VideoRecordingRequest,
    };
    use std::{
        num::NonZeroU32,
        sync::Arc,
        time::{Duration, Instant},
    };
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--allow-screen-capture")) {
        return Err("Requires --allow-screen-capture and a new absolute output path.".into());
    }
    let output = std::path::PathBuf::from(args.next().ok_or("Missing output path")?);
    let display = frame_windows::PlatformBackend
        .enumerate_displays()?
        .into_iter()
        .find(|d| d.is_primary)
        .ok_or("No primary display")?;
    let clock = Arc::new(MasterClock::default());
    let origin = clock.now();
    let mut recording = VideoRecording::start(VideoRecordingRequest {
        display_id: display.id,
        window_id: None,
        config: frame_windows::NativeEncoderConfig::proof(
            display.width,
            display.height,
            NonZeroU32::new(30).unwrap(),
            NonZeroU32::new(1).unwrap(),
        )?,
        encoder_id: None,
        clock,
        origin,
        output: output.clone(),
        camera: None,
        show_cursor: false,
        hdr_main10: false,
    })?;
    recording.enable_metadata_for_source(frame_metadata::MetadataOptions::default())?;
    let started = Instant::now();
    let mut paused = false;
    while started.elapsed() < Duration::from_millis(500) {
        recording.poll()?;
        if !paused && started.elapsed() >= Duration::from_millis(200) {
            recording.pause()?;
            std::thread::sleep(Duration::from_millis(30));
            recording.resume()?;
            paused = true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    recording.request_stop()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while recording.poll()?.phase != VideoRecordingPhase::Drained {
        if Instant::now() > deadline {
            return Err("Drain timeout".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let artifact = recording.finish()?;
    let manifest = recording.session_manifest()?.ok_or("Missing manifest")?;
    let metadata = output
        .parent()
        .unwrap()
        .join(manifest.metadata_file.ok_or("Missing sidecar")?);
    let mut geometry = 0;
    let mut pause_pts = Vec::new();
    let summary = frame_metadata::read_sidecar(&metadata, |record| {
        if let frame_metadata::Record::Event(event) = record {
            match event.event {
                frame_metadata::EventKind::Geometry { .. } => geometry += 1,
                frame_metadata::EventKind::Lifecycle {
                    state: frame_metadata::Lifecycle::Paused | frame_metadata::Lifecycle::Resumed,
                } => pause_pts.push(event.session_ns),
                _ => {}
            }
        }
        Ok(())
    })?;
    if !summary.completed || geometry == 0 || pause_pts.len() != 2 || pause_pts[0] != pause_pts[1] {
        return Err("Metadata closure, geometry or pause mismatch".into());
    }
    println!(
        "artifact={} sidecar={} events={} dropped={} pause_pts={:?}",
        artifact.path,
        metadata.display(),
        summary.events,
        summary.dropped,
        pause_pts
    );
    Ok(())
}
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Windows native check only.");
}
