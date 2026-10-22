//! One short Windows application-command proof; outputs stay in the given folder.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use frame_contracts::*;
    use frame_runtime::recording_control::{EngineEventSink, RecordingControl};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    #[derive(Default)]
    struct Sink {
        events: Mutex<Vec<EngineEvent>>,
    }
    impl EngineEventSink for Sink {
        fn publish(&self, event: EventEnvelope) {
            if matches!(
                event.event,
                EngineEvent::ReplaySaved(_)
                    | EngineEvent::ReplaySaveFailed(_)
                    | EngineEvent::RecordingFinalized(_)
                    | EngineEvent::Error(_)
            ) {
                self.events.lock().unwrap().push(event.event);
            }
        }
    }
    fn wait(control: &RecordingControl, state: RecorderState) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(20);
        while Instant::now() < until {
            let actual = control.published().state;
            if actual == state {
                return Ok(());
            }
            if let RecorderState::Failed(e) = actual {
                return Err(e.message);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!("Timed out waiting for {state:?}"))
    }
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Pass a new absolute evidence folder")?,
    );
    if !root.is_absolute() {
        return Err("Evidence folder must be absolute".into());
    }
    std::fs::create_dir(&root)?;
    let sources = frame_runtime::discovery::snapshot().map_err(|e| e.message)?;
    let display = sources.displays.first().ok_or("No display")?;
    let sink = Arc::new(Sink::default());
    let control = RecordingControl::new(sink.clone());
    control
        .configure_replay(ReplaySettings {
            duration_seconds: 2,
            max_bytes: 64 * 1024 * 1024,
            storage_mib: 128,
        })
        .map_err(|e| e.message)?;
    for replay_only in [true, false] {
        let stem = if replay_only {
            "rolling-only"
        } else {
            "recording"
        };
        let request = RecordingRequest {
            target: CaptureTarget::Display(display.id.clone()),
            fps_mode: FpsMode::Fixed(30.0),
            resolution: Resolution::Native,
            quality: display.required_recording_quality.unwrap_or(Quality::High),
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_routes: vec![],
            camera: None,
            show_cursor: false,
            output: OutputSettings {
                directory: root.to_string_lossy().into_owned(),
                file_stem: stem.into(),
            },
        };
        if replay_only {
            control.start_replay(request)
        } else {
            control.start(request)
        }
        .map_err(|e| e.message)?;
        wait(&control, RecorderState::Recording)?;
        std::thread::sleep(Duration::from_millis(4100));
        let before = control
            .published()
            .metrics
            .map(|m| m.elapsed_ms)
            .unwrap_or(0.0);
        for index in 0..2 {
            control
                .save_replay(root.join(format!("{stem}-save-{index}.mkv")), 1)
                .map_err(|e| e.message)?;
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(control.published().state, RecorderState::Recording);
        let after = control
            .published()
            .metrics
            .map(|m| m.elapsed_ms)
            .unwrap_or(0.0);
        assert!(after > before, "capture clock must advance across export");
        if replay_only {
            control.stop_replay()
        } else {
            control.stop()
        }
        .map_err(|e| e.message)?;
        wait(&control, RecorderState::Idle)?;
        if replay_only {
            assert!(control.published().last_artifact.is_none());
        }
        println!("{stem}: capture advanced from {before:.1} to {after:.1} ms during two saves");
    }
    control.shutdown();
    let events = sink.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EngineEvent::ReplaySaved(_)))
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EngineEvent::RecordingFinalized(_)))
            .count(),
        1
    );
    for event in &*events {
        if let EngineEvent::ReplaySaveFailed(error) | EngineEvent::Error(error) = event {
            return Err(error.message.clone().into());
        }
    }
    let recovered = root.join("restart.freym-session");
    let jobs = frame_runtime::recovery::RecoveryJobs::default();
    let scan = frame_runtime::recovery::discover_available(&root).map_err(|e| e.message)?;
    let archive = scan
        .paths
        .iter()
        .find(|p| p.contains("rolling-only") && p.ends_with("replay.freym-session"))
        .ok_or("Replay archive missing from restart discovery")?;
    let result = jobs
        .begin(&RecorderState::Idle)
        .map_err(|e| e.message)?
        .run_any(std::path::Path::new(archive), &recovered)
        .map_err(|e| e.message)?;
    println!("Restart archive: {}", result.output_path);
    for path in std::fs::read_dir(&root)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mkv"))
    {
        let decoded = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"])
            .output()?;
        if !decoded.status.success() {
            return Err(format!(
                "{}: {}",
                path.display(),
                String::from_utf8_lossy(&decoded.stderr)
            )
            .into());
        }
        println!("Decoded: {}", path.display());
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This probe uses Windows capture.");
}
