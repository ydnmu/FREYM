use frame_contracts::*;
use frame_settings::{
    EncoderCandidate, EncoderLimits, NominalRate, SettingsErrorKind, SettingsStore,
    capabilities::*, model::*, validation::*,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn configuration() -> RecordingConfiguration {
    RecordingRequest {
        target: CaptureTarget::Display(DisplayId("panel".into())),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::High,
        encoder: EncoderPreference::Auto,
        optimization: Some(RecordingOptimization::default()),
        audio_encoding: Default::default(),
        audio_routes: vec![],
        camera: None,

        show_cursor: true,
        output: OutputSettings {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
            file_stem: "session".into(),
        },
    }
    .into()
}

#[test]
fn schema_three_migrates_native_defaults_in_memory_and_preserves_previous_bytes() {
    let dir = Directory::new();
    let store = dir.store();
    let mut old = SettingsDocument::new(configuration());
    old.schema_version = 3;
    old.revision = 7;
    let bytes = serde_json::to_vec_pretty(&old).unwrap();
    fs::write(store.path(), &bytes).unwrap();
    let loaded = store.load_document().unwrap().unwrap();
    assert_eq!(loaded.schema_version, 4);
    assert_eq!(loaded.revision, 7);
    assert!(
        loaded
            .configuration()
            .unwrap()
            .request
            .optimization
            .unwrap()
            .encoder_overrides
            .is_empty()
    );
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    let saved = store.save_document(&loaded, 7).unwrap();
    assert_eq!(saved.schema_version, 4);
    assert_eq!(fs::read(store.backup_path()).unwrap(), bytes);
}

#[test]
fn integrated_audio_video_migration_commit_profile_and_rollback_preserve_both() {
    let directory = Directory::new();
    let store = directory.store();
    let mut c = configuration();
    c.request.audio_routes.push(AudioRouteSelection {
        source_id: AudioSourceId("wasapi-mic:chosen".into()),
        route: AudioRoute::MainMixAndSeparate,
        muted: true,
        controls: AudioSourceControls {
            gain_millidb: -4500,
            balance: 35,
            reconnect: AudioReconnectPolicy::ExactDevice,
        },
    });
    let mut old = SettingsDocument::new(c.clone());
    old.schema_version = 3;
    old.revision = 7;
    old.modes.audio = SettingsMode::Advanced;
    old.capture_profile("studio".into(), "Studio".into())
        .unwrap();
    old.select_profile(Some("studio")).unwrap();
    let old_bytes = serde_json::to_vec_pretty(&old).unwrap();
    fs::write(store.path(), &old_bytes).unwrap();
    let loaded = store.load_document().unwrap().unwrap();
    assert_eq!(loaded.schema_version, 4);
    assert_eq!(loaded.revision, 7);
    assert_eq!(loaded.configuration().unwrap(), c);
    assert_eq!(loaded.profiles, old.profiles);
    assert_eq!(loaded.modes, old.modes);

    c.request.optimization.as_mut().unwrap().encoder_overrides = VideoEncoderOverrides {
        rate_control: Some(VideoRateControl::Cqp),
        qp: Some(21),
        preset: Some(VideoEncoderPreset::P2),
        tuning: Some(VideoEncoderTuning::UltraLowLatency),
        gop_frames: Some(60),
        ..Default::default()
    };
    let mut caps = capabilities(&c);
    let video = caps.video.as_mut().unwrap();
    video.advanced_qualification = Some((
        video.candidates[0].id.clone(),
        c.request.optimization.as_ref().unwrap().encoder_overrides,
    ));
    caps.audio_sources.push(AudioSource {
        id: c.request.audio_routes[0].source_id.clone(),
        kind: AudioSourceKind::Microphone,
        display_name: "Exact microphone fixture".into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route: AudioRoute::Ignore,
        muted: false,
        controls: Default::default(),
        capture_identity: None,
    });
    #[cfg(not(target_os = "windows"))]
    {
        let mut unsupported = loaded.clone();
        unsupported.set_configuration(&c).unwrap();
        let error = store
            .commit(
                &unsupported,
                7,
                &caps,
                &RecorderState::Idle,
                ApplyTiming::Now,
            )
            .unwrap_err();
        assert_eq!(error.kind, SettingsErrorKind::InvalidSelection);
        assert_eq!(
            error.detail,
            "request: Native Advanced overrides are currently implemented only on Windows NVENC."
        );
        assert_eq!(fs::read(store.path()).unwrap(), old_bytes);
        assert_eq!(store.load_document().unwrap().unwrap(), loaded);
        // Exercise the same commit/profile/rollback transaction with portable video intent.
        let optimization = c.request.optimization.as_mut().unwrap();
        optimization.encoder_overrides = VideoEncoderOverrides::default();
        optimization.bitrate_bps = Some(8_000_000);
        let video = caps.video.as_mut().unwrap();
        video.key = VideoProbeKey::from(&c);
        video.advanced_qualification = None;
    }
    let mut edited = loaded.clone();
    edited.set_configuration(&c).unwrap();
    edited.modes.video = SettingsMode::Advanced;
    edited
        .capture_profile("combined".into(), "Combined".into())
        .unwrap();
    edited.select_profile(Some("combined")).unwrap();
    let (saved, _) = store
        .commit(&edited, 7, &caps, &RecorderState::Idle, ApplyTiming::Now)
        .unwrap();
    assert_eq!(saved.revision, 8);
    assert_eq!(store.load_document().unwrap().unwrap(), saved);
    assert_eq!(saved.configuration().unwrap(), c);
    assert_eq!(
        saved.configuration().unwrap().engine_request().unwrap(),
        c.request
    );
    assert_eq!(fs::read(store.backup_path()).unwrap(), old_bytes);
    let saved_bytes = fs::read(store.path()).unwrap();
    assert_eq!(
        store
            .commit(&loaded, 7, &caps, &RecorderState::Idle, ApplyTiming::Now)
            .unwrap_err()
            .kind,
        SettingsErrorKind::Conflict
    );
    assert_eq!(fs::read(store.path()).unwrap(), saved_bytes);
    let restored = store
        .rollback(8, &RecorderState::Idle, ApplyTiming::Now)
        .unwrap();
    assert_eq!(
        restored.configuration().unwrap(),
        loaded.configuration().unwrap()
    );
    assert_eq!(restored.modes, loaded.modes);
    let redone = store
        .rollback(9, &RecorderState::Idle, ApplyTiming::Now)
        .unwrap();
    assert_eq!(redone.configuration().unwrap(), c);
    assert_eq!(redone.profiles, saved.profiles);
    assert_eq!(redone.modes, saved.modes);
}

#[test]
fn advanced_admission_requires_exact_native_combination_and_invalidates_probe_key() {
    let mut c = configuration();
    c.request.optimization.as_mut().unwrap().encoder_overrides = VideoEncoderOverrides {
        rate_control: Some(VideoRateControl::Cqp),
        qp: Some(21),
        ..Default::default()
    };
    let mut caps = capabilities(&c);
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.field == "optimization.encoderOverrides")
    );
    let video = caps.video.as_mut().unwrap();
    video.advanced_qualification = Some((
        video.candidates[0].id.clone(),
        c.request.optimization.as_ref().unwrap().encoder_overrides,
    ));
    let qualified = validate_configuration(&c, &caps);
    if cfg!(target_os = "windows") {
        assert!(qualified.is_valid());
    } else {
        assert!(!qualified.is_valid());
        assert_eq!(qualified.issues.len(), 1);
        assert_eq!(qualified.issues[0].field, "request");
        assert_eq!(qualified.issues[0].code, IssueCode::Invalid);
        assert_eq!(
            qualified.issues[0].message,
            "Native Advanced overrides are currently implemented only on Windows NVENC."
        );
    }
    c.request
        .optimization
        .as_mut()
        .unwrap()
        .encoder_overrides
        .qp = Some(22);
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.code == IssueCode::NeedsProbe)
    );
}

#[test]
fn shared_revision_rebases_video_audio_and_modes_without_losing_other_edits() {
    let base = SettingsDocument::new(configuration());
    let mut video = base.clone();
    let mut c = video.configuration().unwrap();
    c.request.show_cursor = false;
    video.set_configuration(&c).unwrap();
    video.modes.video = SettingsMode::Advanced;
    let mut audio = base.clone();
    let mut c = audio.configuration().unwrap();
    c.request.audio_routes.push(AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId("mic".into()),
        route: AudioRoute::MainMixAndSeparate,
        muted: true,
    });
    audio.set_configuration(&c).unwrap();
    audio.modes.audio = SettingsMode::Advanced;
    audio.revision = 9;
    let merged = SettingsDocument::rebase_edit(&base, &video, &audio).unwrap();
    assert_eq!(merged.revision, 9);
    assert_eq!(merged.modes.video, SettingsMode::Advanced);
    assert_eq!(merged.modes.audio, SettingsMode::Advanced);
    assert!(!merged.configuration().unwrap().request.show_cursor);
    assert_eq!(
        merged.configuration().unwrap().request.audio_routes,
        c.request.audio_routes
    );
}

#[test]
fn concurrent_edits_to_the_same_setting_require_review() {
    let base = SettingsDocument::new(configuration());
    let mut a = base.clone();
    let mut b = base.clone();
    let mut c = a.configuration().unwrap();
    c.request.fps_mode = FpsMode::Fixed(60.);
    a.set_configuration(&c).unwrap();
    c.request.fps_mode = FpsMode::Fixed(120.);
    b.set_configuration(&c).unwrap();
    assert!(
        SettingsDocument::rebase_edit(&base, &a, &b)
            .unwrap_err()
            .contains("fps_mode")
    );
}

#[test]
fn source_processing_survives_shared_commit_profile_reload_and_validation() {
    let directory = Directory::new();
    let store = directory.store();
    let mut c = configuration();
    c.request.audio_routes.push(AudioRouteSelection {
        controls: AudioSourceControls {
            gain_millidb: -4500,
            balance: 35,
            reconnect: AudioReconnectPolicy::ExactDevice,
        },
        source_id: AudioSourceId("wasapi-mic:chosen".into()),
        route: AudioRoute::MainMixAndSeparate,
        muted: false,
    });
    let mut caps = capabilities(&c);
    caps.audio_sources.push(AudioSource {
        controls: Default::default(),
        capture_identity: None,
        id: c.request.audio_routes[0].source_id.clone(),
        kind: AudioSourceKind::Microphone,
        display_name: "Microphone".into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route: AudioRoute::Ignore,
        muted: false,
    });
    let mut document = SettingsDocument::new(c.clone());
    document.modes.audio = SettingsMode::Advanced;
    let (saved, _) = store
        .commit(&document, 0, &caps, &RecorderState::Idle, ApplyTiming::Now)
        .unwrap();
    assert_eq!(
        store
            .load_document()
            .unwrap()
            .unwrap()
            .configuration()
            .unwrap(),
        c
    );
    let mut edited = saved.clone();
    c.request.audio_routes[0].controls.balance = -25;
    edited.set_configuration(&c).unwrap();
    edited
        .capture_profile("voice".into(), "Voice".into())
        .unwrap();
    edited.select_profile(Some("voice")).unwrap();
    let (saved, _) = store
        .commit(
            &edited,
            saved.revision,
            &caps,
            &RecorderState::Idle,
            ApplyTiming::Now,
        )
        .unwrap();
    assert_eq!(
        saved.configuration().unwrap().request.audio_routes[0].controls,
        c.request.audio_routes[0].controls
    );
    assert_eq!(audio_easy_summary(&c), AudioEasySummary::Custom);
    let prior = fs::read(store.path()).unwrap();
    c.request.audio_routes[0].controls.gain_millidb = 24001;
    let mut invalid = saved.clone();
    invalid.set_configuration(&c).unwrap();
    assert!(
        store
            .commit(
                &invalid,
                saved.revision,
                &caps,
                &RecorderState::Idle,
                ApplyTiming::Now
            )
            .is_err()
    );
    assert_eq!(fs::read(store.path()).unwrap(), prior);
}

#[test]
fn reverting_an_inflight_edit_is_reapplied_against_the_submitted_draft() {
    let initial = SettingsDocument::new(configuration());
    let mut submitted = initial.clone();
    let mut c = submitted.configuration().unwrap();
    c.request.show_cursor = false;
    submitted.set_configuration(&c).unwrap();
    let queued_revert = initial.clone();
    let mut committed = submitted.clone();
    committed.revision = 2;
    committed.modes.audio = SettingsMode::Advanced;
    let merged = SettingsDocument::rebase_edit(&submitted, &queued_revert, &committed).unwrap();
    assert!(merged.configuration().unwrap().request.show_cursor);
    assert_eq!(merged.modes.audio, SettingsMode::Advanced);
    assert_eq!(merged.revision, 2);
}

#[test]
fn every_host_accepts_stems_only_audio_before_native_preparation() {
    let mut c = configuration();
    c.request.audio_routes.push(AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId("mic".into()),
        route: AudioRoute::SeparateOnly,
        muted: false,
    });
    assert_eq!(
        c.engine_request().unwrap().audio_routes,
        c.request.audio_routes
    );
    assert!(
        frame_settings::audio_admission::validate_audio_topology(&c.request.audio_routes).is_ok()
    );
}

#[test]
fn configured_audio_survives_profiles_legacy_load_and_engine_admission() {
    let directory = Directory::new();
    let store = directory.store();
    let mut c = configuration();
    c.audio.set_quality(AudioQuality::High);
    c.audio.channels = AudioChannels::Mono;
    let mut document = SettingsDocument::new(c.clone());
    document
        .capture_profile("audio".into(), "Audio".into())
        .unwrap();
    document.select_profile(Some("audio")).unwrap();
    let saved = store.save_document(&document, 0).unwrap();
    assert_eq!(saved.configuration().unwrap().audio, c.audio);
    let request = store.load().unwrap().unwrap();
    assert_eq!(request.audio_encoding, c.audio);
    assert_eq!(c.engine_request().unwrap().audio_encoding, c.audio);
    store.save(&request).unwrap();
    assert_eq!(
        store
            .load_document()
            .unwrap()
            .unwrap()
            .configuration()
            .unwrap()
            .audio,
        c.audio
    );
    assert_eq!(c.audio.quality(), AudioQuality::High);
    let mut legacy = request;
    legacy.audio_encoding = Default::default();
    store.save(&legacy).unwrap();
    assert_eq!(store.load().unwrap().unwrap().audio_encoding, c.audio);
    c.audio.bitrate_bps = 32_000;
    assert_eq!(c.audio.quality(), AudioQuality::Custom);
    assert!(c.engine_request().is_ok());
    for rate in [44_100, 96_000] {
        c.audio.sample_rate_hz = rate;
        assert!(c.engine_request().unwrap_err().message.contains("48 kHz"));
    }
}

#[test]
fn native_audio_admission_rejects_aliases_and_missing_sources_but_preserves_stems() {
    use frame_settings::audio_admission::validate_audio_selection;
    let source = |id: &str, pid| AudioSource {
        controls: Default::default(),
        capture_identity: None,
        id: AudioSourceId(id.into()),
        kind: AudioSourceKind::Application,
        display_name: id.into(),
        process_id: Some(pid),
        app_identifier: None,
        is_active: true,
        route: AudioRoute::Ignore,
        muted: false,
    };
    let routes = |ids: &[&str]| {
        ids.iter()
            .map(|id| AudioRouteSelection {
                controls: Default::default(),
                source_id: AudioSourceId((*id).into()),
                route: AudioRoute::MainMixAndSeparate,
                muted: true,
            })
            .collect::<Vec<_>>()
    };
    let sources = [
        source("wasapi-session:one", 42),
        source("wasapi-session:two", 42),
    ];
    assert!(
        validate_audio_selection(
            &routes(&["wasapi-session:one", "wasapi-session:two"]),
            &sources,
            true,
            true
        )
        .unwrap_err()
        .message
        .contains("overlapping")
    );
    assert!(validate_audio_selection(&routes(&["missing"]), &sources, true, true).is_err());
    assert!(
        validate_audio_selection(&routes(&["wasapi-session:one"]), &sources, true, true).is_ok()
    );
    assert!(
        validate_audio_selection(&routes(&["wasapi-session:one"]), &sources, false, true).is_err()
    );
}

#[test]
fn shared_admission_rejects_cbr_substitution_after_a_valid_native_probe() {
    let mut c = configuration();
    c.request.optimization = None;
    c.request.quality = Quality::High;
    let mut caps = capabilities(&c);
    caps.video.as_mut().unwrap().candidates[0].limits.vbr = false;
    let report = validate_configuration(&c, &caps);
    assert!(report.issues.iter().any(|i| i.message.contains("VBR")));
}
fn capabilities(c: &RecordingConfiguration) -> SettingsCapabilities {
    let mut engine = EngineCapabilities::unavailable();
    engine.can_record = true;
    engine.display_capture = true;
    engine.fixed_fps = true;
    engine.region_capture = true;
    engine.per_app_audio = true;
    engine.microphone = true;
    SettingsCapabilities {
        os: OperatingSystem::Windows,
        os_build: Some(26100),
        engine,
        displays: vec![DisplayInfo {
            id: DisplayId("panel".into()),
            name: "Panel".into(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            scale_factor: 1.,
            refresh_rate_hz: Some(60.),
            is_primary: true,
            hdr_available: false,
            required_recording_quality: None,
        }],
        windows: vec![],
        audio_sources: vec![],
        audio_devices: vec![],
        cameras: vec![],
        gpus: vec![GpuCapability {
            id: "gpu".into(),
            name: "Qualified fixture".into(),
            vendor_id: 0x10DE,
            driver_version: None,
        }],
        video: Some(VideoCapabilities {
            advanced_qualification: None,
            key: c.into(),
            capture_gpu_id: Some("gpu".into()),
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: NominalRate::recover_exact(60.).unwrap(),
            hdr_source: false,
            default_codec: "h264".into(),
            candidates: vec![EncoderCandidate {
                id: "nvidia-nvenc-h264-d3d11:luid=fixture".into(),
                codec: "h264".into(),
                hardware: true,
                limits: EncoderLimits {
                    max_width: 1920,
                    max_height: 1080,
                    cbr: true,
                    vbr: true,
                },
            }],
        }),
        diagnostics: vec![],
    }
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "freym-settings-core-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> SettingsStore {
        SettingsStore::in_directory(&self.0)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn modes_and_easy_presets_preserve_advanced_values_and_project_custom() {
    let mut c = configuration();
    c.request.fps_mode = FpsMode::Fixed(119.88);
    c.request.encoder = EncoderPreference::Explicit("retained encoder".into());
    c.request.optimization.as_mut().unwrap().bitrate_bps = Some(17_000_000);
    c.request.optimization.as_mut().unwrap().automatic_quality = false;
    c.request.audio_routes.push(AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId("mic".into()),
        route: AudioRoute::MainMixAndSeparate,
        muted: true,
    });
    let mut document = SettingsDocument::new(c.clone());
    document.modes.video = SettingsMode::Advanced;
    document.modes.audio = SettingsMode::Easy;
    document.modes.video = SettingsMode::Easy;
    assert_eq!(document.configuration().unwrap(), c);
    assert_eq!(video_easy_summary(&c), RecordingPreset::Custom);
    assert_eq!(audio_easy_summary(&c), AudioEasySummary::Custom);
    set_video_easy_preset(&mut c, RecordingPreset::Gaming);
    assert_eq!(c.request.fps_mode, FpsMode::Fixed(119.88));
    assert_eq!(
        c.request.optimization.as_ref().unwrap().bitrate_bps,
        Some(17_000_000)
    );
    assert!(!c.request.optimization.as_ref().unwrap().automatic_quality);
    assert_eq!(video_easy_summary(&c), RecordingPreset::Custom);
    let default = configuration();
    assert_eq!(video_easy_summary(&default), RecordingPreset::Desktop);
    assert_eq!(audio_easy_summary(&default), AudioEasySummary::Automatic);
}

#[test]
fn profile_resolution_clearable_overrides_and_reset_are_one_model() {
    let mut d = SettingsDocument::new(configuration());
    let mut c = d.configuration().unwrap();
    c.request.quality = Quality::Maximum;
    d.set_configuration(&c).unwrap();
    d.capture_profile("high".into(), "High quality".into())
        .unwrap();
    d.select_profile(Some("high")).unwrap();
    assert_eq!(d.configuration().unwrap(), c);
    c.request.optimization = None;
    d.set_configuration(&c).unwrap();
    let json = serde_json::to_vec(&d).unwrap();
    let restored: SettingsDocument = serde_json::from_slice(&json).unwrap();
    assert_eq!(restored.configuration().unwrap(), c);
    assert_eq!(restored.overrides.optimization, Override::Set(None));
    d.reset_overrides();
    assert!(d.configuration().unwrap().request.optimization.is_some());
    d.select_profile(None).unwrap();
    assert_eq!(d.configuration().unwrap(), configuration());
    assert!(d.select_profile(Some("missing")).is_err());
}

#[test]
fn v2_migration_keeps_all_engine_preferences_and_legacy_save_keeps_new_state() {
    let directory = Directory::new();
    let store = directory.store();
    let mut c = configuration();
    c.request.show_cursor = false;
    c.request.fps_mode = FpsMode::Fixed(119.88);
    c.request.optimization.as_mut().unwrap().codec = RecordingCodec::Av1;
    c.request.audio_routes.push(AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId("absent-but-retained".into()),
        route: AudioRoute::SeparateOnly,
        muted: true,
    });
    let legacy =
        serde_json::to_vec(&serde_json::json!({ "schemaVersion": 2, "request": c.request }))
            .unwrap();
    fs::write(store.path(), &legacy).unwrap();
    let mut doc = store.load_document().unwrap().unwrap();
    assert_eq!(doc.configuration().unwrap(), c);
    assert_eq!(fs::read(store.path()).unwrap(), legacy);
    doc.modes.audio = SettingsMode::Advanced;
    doc.capture_profile("legacy".into(), "Legacy".into())
        .unwrap();
    let saved = store.save_document(&doc, 0).unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(fs::read(store.backup_path()).unwrap(), legacy);
    let mut request = c.request;
    request.quality = Quality::Standard;
    store.save(&request).unwrap();
    let after = store.load_document().unwrap().unwrap();
    assert_eq!(after.modes.audio, SettingsMode::Advanced);
    assert!(after.profiles.contains_key("legacy"));
    assert_eq!(store.load().unwrap(), Some(request));
    assert_eq!(after.revision, 2);
}

#[test]
fn stale_drafts_invalid_profiles_and_future_documents_never_overwrite_current() {
    let directory = Directory::new();
    let store = directory.store();
    let saved = store
        .save_document(&SettingsDocument::new(configuration()), 0)
        .unwrap();
    let bytes = fs::read(store.path()).unwrap();
    assert_eq!(
        store.save_document(&saved, 0).unwrap_err().kind,
        SettingsErrorKind::Conflict
    );
    let mut invalid = saved.clone();
    invalid.active_profile = Some("missing".into());
    assert_eq!(
        store.save_document(&invalid, 1).unwrap_err().kind,
        SettingsErrorKind::InvalidSelection
    );
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    let future = br#"{"schemaVersion":5,"future":true}"#;
    fs::write(store.path(), future).unwrap();
    assert_eq!(
        store.save(&configuration().request).unwrap_err().kind,
        SettingsErrorKind::UnsupportedVersion
    );
    assert_eq!(fs::read(store.path()).unwrap(), future);
    fs::write(store.path(), b"corrupt").unwrap();
    assert_eq!(
        store.save_document(&saved, 1).unwrap_err().kind,
        SettingsErrorKind::Corrupt
    );
    assert_eq!(fs::read(store.path()).unwrap(), b"corrupt");
}

#[test]
fn rollback_is_reversible_and_respects_recording_state() {
    let directory = Directory::new();
    let store = directory.store();
    let first = store
        .save_document(&SettingsDocument::new(configuration()), 0)
        .unwrap();
    let mut next = first.clone();
    next.overrides.show_cursor = Override::Set(false);
    let second = store.save_document(&next, 1).unwrap();
    assert!(
        store
            .rollback(2, &RecorderState::Paused, ApplyTiming::Now)
            .is_err()
    );
    assert_eq!(store.load_document().unwrap().unwrap(), second);
    let restored = store
        .rollback(2, &RecorderState::Recording, ApplyTiming::NextRecording)
        .unwrap();
    assert!(restored.configuration().unwrap().request.show_cursor);
    assert_eq!(restored.revision, 3);
    let undone = store
        .rollback(3, &RecorderState::Idle, ApplyTiming::Now)
        .unwrap();
    assert!(!undone.configuration().unwrap().request.show_cursor);
    assert_eq!(undone.revision, 4);
}

#[test]
fn cooperative_writers_admit_exactly_one_draft() {
    let directory = Directory::new();
    let store = directory.store();
    let saved = store
        .save_document(&SettingsDocument::new(configuration()), 0)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|i| {
            let store = store.clone();
            let mut draft = saved.clone();
            let barrier = barrier.clone();
            draft.modes.video = if i == 0 {
                SettingsMode::Advanced
            } else {
                SettingsMode::Easy
            };
            std::thread::spawn(move || {
                barrier.wait();
                store.save_document(&draft, 1)
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, Err(e) if e.kind == SettingsErrorKind::Conflict))
            .count(),
        1
    );
    assert_eq!(store.load_document().unwrap().unwrap().revision, 2);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
}

#[test]
fn admission_rejects_missing_or_stale_evidence_and_unsupported_dependencies() {
    let mut c = configuration();
    let mut caps = capabilities(&c);
    assert!(validate_configuration(&c, &caps).is_valid());
    c.request.fps_mode = FpsMode::Fixed(120.);
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.code == IssueCode::NeedsProbe)
    );
    caps.video.as_mut().unwrap().key = (&c).into();
    c.request.optimization.as_mut().unwrap().codec = RecordingCodec::Hevc;
    caps.video.as_mut().unwrap().key = (&c).into();
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.code == IssueCode::Unsupported)
    );
    c = configuration();
    c.request.resolution = Resolution::Exact(Dimensions {
        width: 1280,
        height: 720,
    });
    caps = capabilities(&c);
    assert!(!validate_configuration(&c, &caps).is_valid());
    c = configuration();
    caps = capabilities(&c);
    caps.video = None;
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.code == IssueCode::NeedsProbe)
    );
    caps = capabilities(&c);
    caps.video.as_mut().unwrap().hdr_source = true;
    c.request.target = CaptureTarget::Region(Region {
        display_id: DisplayId("panel".into()),
        x: 0,
        y: 0,
        width: 1280,
        height: 720,
    });
    caps.video.as_mut().unwrap().key = (&c).into();
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.message.contains("HDR"))
    );
}

#[test]
fn unsupported_audio_never_disappears_at_legacy_or_native_admission() {
    let directory = Directory::new();
    let store = directory.store();
    let mut c = configuration();
    c.audio.codec = AudioCodec::Aac;
    let caps = capabilities(&c);
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.field == "audio")
    );
    assert!(c.engine_request().is_err());
    store
        .save_document(&SettingsDocument::new(c.clone()), 0)
        .unwrap();
    assert_eq!(
        store
            .load_document()
            .unwrap()
            .unwrap()
            .configuration()
            .unwrap(),
        c
    );
    assert_eq!(
        store.load().unwrap_err().kind,
        SettingsErrorKind::InvalidSelection
    );
    c = configuration();
    c.request.audio_routes.push(AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId("unplugged".into()),
        route: AudioRoute::MainMix,
        muted: false,
    });
    assert!(
        validate_configuration(&c, &capabilities(&c))
            .issues
            .iter()
            .any(|i| i.code == IssueCode::SourceUnavailable)
    );
}

#[test]
fn live_commits_allow_modes_but_reject_engine_changes_unless_deferred() {
    let directory = Directory::new();
    let store = directory.store();
    let c = configuration();
    let caps = capabilities(&c);
    let (first, _) = store
        .commit(
            &SettingsDocument::new(c),
            0,
            &caps,
            &RecorderState::Idle,
            ApplyTiming::Now,
        )
        .unwrap();
    let mut next = first.clone();
    next.modes.audio = SettingsMode::Advanced;
    let (second, effect) = store
        .commit(&next, 1, &caps, &RecorderState::Preparing, ApplyTiming::Now)
        .unwrap();
    assert_eq!(effect, ChangeEffect::PresentationOnly);
    next = second;
    next.overrides.show_cursor = Override::Set(false);
    for state in [
        RecorderState::Preparing,
        RecorderState::Recording,
        RecorderState::Paused,
        RecorderState::Finalizing,
    ] {
        assert!(
            store
                .commit(&next, 2, &caps, &state, ApplyTiming::Now)
                .is_err()
        );
    }
    let (third, effect) = store
        .commit(
            &next,
            2,
            &caps,
            &RecorderState::Recording,
            ApplyTiming::NextRecording,
        )
        .unwrap();
    assert_eq!(effect, ChangeEffect::NextRecording);
    assert_eq!(third.revision, 3);
}

#[test]
fn corrupt_recovery_requires_exact_bytes_and_preserves_the_displaced_file() {
    let directory = Directory::new();
    let store = directory.store();
    let first = store
        .save_document(&SettingsDocument::new(configuration()), 0)
        .unwrap();
    store.save_document(&first, 1).unwrap();
    fs::write(store.path(), b"broken settings").unwrap();
    assert_eq!(
        store
            .recover_backup(b"stale bytes", &RecorderState::Idle, ApplyTiming::Now)
            .unwrap_err()
            .kind,
        SettingsErrorKind::Conflict
    );
    assert!(
        store
            .recover_backup(
                b"broken settings",
                &RecorderState::Finalizing,
                ApplyTiming::Now
            )
            .is_err()
    );
    let restored = store
        .recover_backup(b"broken settings", &RecorderState::Idle, ApplyTiming::Now)
        .unwrap();
    assert_eq!(restored.configuration().unwrap(), configuration());
    assert!(restored.revision > 2);
    assert_eq!(fs::read(store.backup_path()).unwrap(), b"broken settings");
    fs::write(store.path(), br#"{"schemaVersion":99}"#).unwrap();
    assert_eq!(
        store
            .recover_backup(
                br#"{"schemaVersion":99}"#,
                &RecorderState::Idle,
                ApplyTiming::Now
            )
            .unwrap_err()
            .kind,
        SettingsErrorKind::UnsupportedVersion
    );
}

#[test]
fn failed_write_and_bad_device_format_keep_prior_preferences() {
    let directory = Directory::new();
    let store = directory.store();
    let first = store
        .save_document(&SettingsDocument::new(configuration()), 0)
        .unwrap();
    let bytes = fs::read(store.path()).unwrap();
    // A read-only/unreplaceable backup destination fails before the primary changes.
    fs::create_dir(store.backup_path()).unwrap();
    assert_eq!(
        store.save_document(&first, 1).unwrap_err().kind,
        SettingsErrorKind::Write
    );
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
    let mut c = configuration();
    c.request.audio_routes.push(AudioRouteSelection {
        controls: Default::default(),
        source_id: AudioSourceId("mic".into()),
        route: AudioRoute::MainMix,
        muted: false,
    });
    let mut caps = capabilities(&c);
    caps.audio_sources.push(AudioSource {
        controls: Default::default(),
        capture_identity: None,
        id: AudioSourceId("mic".into()),
        kind: AudioSourceKind::Microphone,
        display_name: "Microphone".into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route: AudioRoute::Ignore,
        muted: false,
    });
    caps.audio_devices.push((
        AudioSourceId("mic".into()),
        AudioDeviceQualification::Unavailable("Unsupported 6-channel mix format".into()),
    ));
    assert!(
        validate_configuration(&c, &caps)
            .issues
            .iter()
            .any(|i| i.message.contains("6-channel"))
    );
}
