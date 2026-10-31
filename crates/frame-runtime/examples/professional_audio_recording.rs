//! Short, explicit native multi-source acceptance in an isolated directory.
//! Usage: professional_audio_recording --allow-screen-capture DIR desktop|app [PID]
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
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--allow-screen-capture") {
        return Err("Explicit --allow-screen-capture required".into());
    }
    let directory = PathBuf::from(args.next().ok_or("Missing evidence directory")?);
    if !directory.is_absolute() {
        return Err("Absolute evidence directory required".into());
    }
    let mode = args.next().ok_or("Missing desktop/app mode")?;
    let pid = args.next().map(|s| s.parse::<u32>()).transpose()?;
    std::fs::create_dir(&directory)?;
    let snapshot = frame_runtime::discovery::snapshot()?;
    std::fs::write(
        directory.join("discovery.json"),
        serde_json::to_vec_pretty(&snapshot.audio_sources)?,
    )?;
    let display = snapshot
        .displays
        .iter()
        .find(|d| d.is_primary)
        .ok_or("No primary display")?;
    let mic = snapshot
        .audio_sources
        .iter()
        .find(|s| s.kind == AudioSourceKind::Microphone && s.is_active)
        .ok_or("No microphone")?;
    let output = snapshot
        .audio_sources
        .iter()
        .find(|s| {
            s.is_active
                && match mode.as_str() {
                    "desktop" => s.kind == AudioSourceKind::System,
                    "app" | "triple" => {
                        s.kind == AudioSourceKind::Application && s.process_id == pid
                    }
                    _ => false,
                }
        })
        .ok_or("Requested output source unavailable")?;
    let mut routes: Vec<_> = [mic, output]
        .into_iter()
        .map(|s| AudioRouteSelection {
            source_id: s.id.clone(),
            route: AudioRoute::MainMixAndSeparate,
            muted: false,
            controls: Default::default(),
        })
        .collect();
    // Native PCM must actually consume these non-default controls.
    routes[1].controls.gain_millidb = -3000;
    routes[1].controls.balance = 40;
    if mode == "triple" {
        let complement = snapshot
            .audio_sources
            .iter()
            .find(|s| s.id.0.strip_prefix("wasapi-rest:") == Some(&output.id.0))
            .ok_or("No complement for app")?;
        routes.push(AudioRouteSelection {
            source_id: complement.id.clone(),
            route: AudioRoute::MainMixAndSeparate,
            muted: false,
            controls: Default::default(),
        });
        let ordinary_desktop = snapshot
            .audio_sources
            .iter()
            .find(|s| s.kind == AudioSourceKind::System && s.id.0.starts_with("wasapi-loopback:"))
            .ok_or("No desktop")?;
        let mut overlapping = routes[..2].to_vec();
        overlapping.push(AudioRouteSelection {
            source_id: ordinary_desktop.id.clone(),
            route: AudioRoute::MainMix,
            muted: true,
            controls: Default::default(),
        });
        let rejected = frame_settings::audio_admission::validate_audio_selection(
            &overlapping,
            &snapshot.audio_sources,
            true,
            true,
        )
        .unwrap_err();
        std::fs::write(
            directory.join("ordinary-overlap-refused.txt"),
            rejected.message,
        )?;
    }
    let config: RecordingConfiguration = RecordingRequest {
        target: CaptureTarget::Display(display.id.clone()),
        fps_mode: FpsMode::Fixed(30.0),
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: Some(RecordingOptimization {
            automatic_quality: false,
            ..Default::default()
        }),
        audio_encoding: Default::default(),
        audio_routes: routes,
        camera: None,

        show_cursor: true,
        output: OutputSettings {
            directory: directory.to_string_lossy().into_owned(),
            file_stem: "Audio_{timestamp}".into(),
        },
    }
    .into();
    let caps = frame_runtime::settings::discover(&config, &RecorderState::Idle)?;
    let report = validate_configuration(&config, &caps);
    if !report.is_valid() {
        return Err(format!("Preflight refused: {:?}", report.issues).into());
    }
    let mut document = SettingsDocument::new(config);
    document.modes.audio = SettingsMode::Advanced;
    let store = SettingsStore::in_directory(&directory);
    let (document, _) =
        store.commit(&document, 0, &caps, &RecorderState::Idle, ApplyTiming::Now)?;
    assert_eq!(store.load_document()?.unwrap(), document);
    let sink = Arc::new(Evidence::default());
    let control = RecordingControl::new(sink.clone());
    control.restore_routes(&document.configuration()?.request.audio_routes);
    let output_id = output.id.clone();
    control.observe_sources(snapshot.audio_sources);
    control.start_configuration(&document.configuration()?)?;
    let deadline = Instant::now() + Duration::from_secs(50);
    let mut since = None;
    let mut muted = false;
    let mut restored = false;
    loop {
        let p = control.published();
        match p.state {
            RecorderState::Recording => {
                let t = since.get_or_insert_with(Instant::now);
                if !muted && t.elapsed() >= Duration::from_secs(3) {
                    control.update_mute(output_id.clone(), true)?;
                    muted = true;
                }
                if !restored && t.elapsed() >= Duration::from_secs(5) {
                    control.update_mute(output_id.clone(), false)?;
                    restored = true;
                }
                if t.elapsed() >= Duration::from_secs(16) {
                    control.stop()?;
                }
            }
            RecorderState::Failed(e) => {
                std::fs::write(
                    directory.join("events.json"),
                    serde_json::to_vec_pretty(&*sink.0.lock().unwrap())?,
                )?;
                return Err(format!("Recording failed: {e:?}").into());
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
                println!("artifact={} integrity={integrity:?}", artifact.path);
                return Ok(());
            }
            _ => {}
        }
        if Instant::now() > deadline {
            let _ = control.stop();
            return Err("Recording deadline exceeded".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
