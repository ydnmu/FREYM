//! Short native Advanced acceptance: isolated settings, production controller and actual media. Uses only an isolated evidence directory.
use frame_contracts::*;
use frame_runtime::recording_control::{EngineEventSink, RecordingControl};
use frame_settings::{SettingsStore, model::*, validation::*};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Evidence(Mutex<Vec<EventEnvelope>>);
impl EngineEventSink for Evidence {
    fn publish(&self, event: EventEnvelope) {
        self.0.lock().unwrap().push(event);
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--frame-mf-helper") {
        std::process::exit(frame_windows::run_mf_helper());
    }
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--allow-screen-capture")) {
        return Err(
            "Requires --allow-screen-capture and a new absolute evidence directory.".into(),
        );
    }
    let directory = PathBuf::from(args.next().ok_or("Missing evidence directory")?);
    let codec = args.next().ok_or("Missing h264/av1 codec")?;
    if codec != "h264" && codec != "av1" {
        return Err("Codec must be h264 or av1".into());
    }
    let av1 = codec == "av1";
    let overrides: VideoEncoderOverrides = serde_json::from_slice(&std::fs::read(
        args.next().ok_or("Missing overrides JSON")?,
    )?)?;
    let bitrate = args
        .next()
        .map(|v| v.to_string_lossy().parse::<u32>())
        .transpose()?;

    if !directory.is_absolute() {
        return Err("Evidence directory must be absolute.".into());
    }
    std::fs::create_dir(&directory)?;
    let snapshot = frame_runtime::discovery::snapshot()?;
    let display = snapshot
        .displays
        .iter()
        .find(|d| d.is_primary)
        .ok_or("No primary display")?;
    let routes = Vec::new();
    let expected_tracks = if routes.is_empty() { 0 } else { 2 };
    let config: RecordingConfiguration = RecordingRequest {
        target: CaptureTarget::Display(display.id.clone()),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: Some(RecordingOptimization {
            automatic_quality: true,
            encoder_overrides: overrides,
            bitrate_bps: bitrate,
            codec: if av1 {
                RecordingCodec::Av1
            } else {
                RecordingCodec::H264
            },
            ..Default::default()
        }),
        audio_routes: routes,
        camera: None,
        show_cursor: true,
        output: OutputSettings {
            directory: directory.to_string_lossy().into_owned(),
            file_stem: "Advanced_{timestamp}".into(),
        },
    }
    .into();
    let caps = frame_runtime::settings::discover(&config, &RecorderState::Idle)?;
    let report = validate_configuration(&config, &caps);
    if !report.is_valid() {
        return Err(format!("Preflight refused: {:?}", report.issues).into());
    }
    let mut document = SettingsDocument::new(config);
    document.modes.video = SettingsMode::Advanced;
    document.modes.audio = SettingsMode::Advanced;
    let store = SettingsStore::in_directory(&directory);
    let (document, _) =
        store.commit(&document, 0, &caps, &RecorderState::Idle, ApplyTiming::Now)?;
    assert_eq!(store.load_document()?.unwrap(), document);
    let sink = Arc::new(Evidence::default());
    let control = RecordingControl::new(sink.clone());
    control.restore_routes(&document.configuration()?.request.audio_routes);
    control.observe_sources(snapshot.audio_sources);
    control.start_configuration(&document.configuration()?)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut recording_since = None;
    loop {
        let p = control.published();
        match p.state {
            RecorderState::Recording => {
                let since = recording_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(5) {
                    control.stop()?;
                }
            }
            RecorderState::Failed(error) => {
                std::fs::write(
                    directory.join("events.json"),
                    serde_json::to_vec_pretty(&*sink.0.lock().unwrap())?,
                )?;
                return Err(format!("Native recording failed: {error:?}").into());
            }
            RecorderState::Idle if p.last_artifact.is_some() => {
                let artifact = p.last_artifact.unwrap();
                let integrity = frame_runtime::recording_health::verify_finalized(
                    std::path::Path::new(&artifact.path),
                )?;
                std::fs::write(
                    directory.join("events.json"),
                    serde_json::to_vec_pretty(&*sink.0.lock().unwrap())?,
                )?;
                println!(
                    "artifact={} expected_audio_tracks={} integrity={integrity:?}",
                    artifact.path, expected_tracks
                );
                return Ok(());
            }
            _ => {}
        }
        if Instant::now() > deadline {
            let _ = control.stop();
            return Err("Native recording deadline exceeded.".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
