use frame_contracts::*;
use frame_settings::audio_admission::*;
fn source(
    id: &str,
    kind: AudioSourceKind,
    process: Option<&str>,
    ancestors: &[&str],
) -> AudioSource {
    AudioSource {
        id: AudioSourceId(id.into()),
        kind,
        display_name: id.into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route: AudioRoute::MainMixAndSeparate,
        muted: false,
        controls: Default::default(),
        capture_identity: Some(AudioCaptureIdentity {
            endpoint: None,
            process_instance: process.map(str::to_owned),
            ancestor_instances: ancestors.iter().map(|s| (*s).to_owned()).collect(),
        }),
    }
}
#[test]
fn known_process_tree_overlap_and_pid_reuse_are_distinguished() {
    let parent = source(
        "wasapi-session:parent",
        AudioSourceKind::Application,
        Some("123:100"),
        &[],
    );
    let child = source(
        "wasapi-session:child",
        AudioSourceKind::Application,
        Some("456:200"),
        &["123:100"],
    );
    let sibling = source(
        "wasapi-session:sibling",
        AudioSourceKind::Application,
        Some("789:200"),
        &["123:100"],
    );
    assert!(overlaps(&parent, &child));
    assert!(overlaps(&child, &parent));
    assert!(!overlaps(&sibling, &child));
    let reused = source(
        "wasapi-session:reused",
        AudioSourceKind::Application,
        Some("123:300"),
        &[],
    );
    assert!(!overlaps(&reused, &child));
}
#[test]
fn endpoint_aliases_and_muted_desktop_application_overlap_are_refused() {
    let mut a = source("wasapi-loopback:A", AudioSourceKind::System, None, &[]);
    let mut b = source("wasapi-loopback:alias", AudioSourceKind::System, None, &[]);
    a.capture_identity.as_mut().unwrap().endpoint = Some("device-a".into());
    b.capture_identity.as_mut().unwrap().endpoint = Some("DEVICE-A".into());
    assert!(overlaps(&a, &b));
    let mut app = source(
        "wasapi-session:app",
        AudioSourceKind::Application,
        Some("12:10"),
        &[],
    );
    app.muted = true;
    let sources = [a, app];
    let routes: Vec<_> = sources
        .iter()
        .map(|s| AudioRouteSelection {
            source_id: s.id.clone(),
            route: s.route,
            muted: s.muted,
            controls: s.controls,
        })
        .collect();
    assert!(
        validate_audio_selection(&routes, &sources, true, true)
            .unwrap_err()
            .message
            .contains("overlapping")
    );
}
#[test]
fn desktop_and_microphone_are_supported_without_changing_legacy_defaults() {
    let legacy: AudioRouteSelection =
        serde_json::from_str(r#"{"sourceId":"mic","route":"mainMix","muted":false}"#).unwrap();
    assert_eq!(legacy.controls, AudioSourceControls::default());
    let sources = [
        source("wasapi-loopback:output", AudioSourceKind::System, None, &[]),
        source("wasapi-mic:input", AudioSourceKind::Microphone, None, &[]),
    ];
    let routes: Vec<_> = sources
        .iter()
        .map(|s| AudioRouteSelection {
            source_id: s.id.clone(),
            route: s.route,
            muted: false,
            controls: Default::default(),
        })
        .collect();
    validate_audio_selection(&routes, &sources, true, true).unwrap();
    let mut unavailable = sources[0].clone();
    unavailable.id.0 = "unknown-system-source".into();
    let r = AudioRouteSelection {
        source_id: unavailable.id.clone(),
        ..routes[0].clone()
    };
    assert!(validate_audio_selection(&[r], &[unavailable], true, true).is_err());
}

#[test]
fn process_complement_is_disjoint_only_from_the_excluded_tree() {
    let rest = source(
        "wasapi-rest:wasapi-session:app",
        AudioSourceKind::System,
        Some("123:100"),
        &[],
    );
    let app = source(
        "wasapi-session:app",
        AudioSourceKind::Application,
        Some("123:100"),
        &[],
    );
    let child = source(
        "wasapi-session:child",
        AudioSourceKind::Application,
        Some("456:200"),
        &["123:100"],
    );
    let other = source(
        "wasapi-session:other",
        AudioSourceKind::Application,
        Some("789:200"),
        &[],
    );
    let desktop = source("wasapi-loopback:output", AudioSourceKind::System, None, &[]);
    assert!(!overlaps(&rest, &app));
    assert!(!overlaps(&app, &rest));
    assert!(!overlaps(&rest, &child));
    assert!(overlaps(&rest, &other));
    assert!(overlaps(&rest, &desktop));
    assert!(overlaps(&rest, &rest));
    let mic = source("wasapi-mic:input", AudioSourceKind::Microphone, None, &[]);
    let sources = [rest, app, mic];
    let routes: Vec<_> = sources
        .iter()
        .map(|s| AudioRouteSelection {
            source_id: s.id.clone(),
            route: s.route,
            muted: false,
            controls: Default::default(),
        })
        .collect();
    validate_audio_selection(&routes, &sources, true, true).unwrap();
    assert!(validate_audio_selection(&routes, &sources, false, true).is_err());
}
