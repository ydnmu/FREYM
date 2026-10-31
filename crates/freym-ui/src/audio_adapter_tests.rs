use super::*;
use frame_audio::{AlignedPcmProcessor, PcmBlock, TrackKind};
use frame_clock::MediaTimestamp;
use std::num::{NonZeroU16, NonZeroU32};

fn source(id: &str, kind: AudioSourceKind, next: AudioRoute) -> AudioSource {
    AudioSource {
        controls: Default::default(),
        capture_identity: None,
        id: AudioSourceId(id.into()),
        kind,
        display_name: id.into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route: next,
        muted: false,
    }
}

#[test]
fn matrix_cells_map_to_all_real_route_variants() {
    for value in [
        AudioRoute::Ignore,
        AudioRoute::MainMix,
        AudioRoute::SeparateOnly,
        AudioRoute::MainMixAndSeparate,
    ] {
        assert_eq!(route(main(value), individual(value)), value);
    }
}

#[test]
fn separate_only_and_video_only_are_valid_without_rewriting_routes() {
    let sources = [source(
        "mic",
        AudioSourceKind::Microphone,
        AudioRoute::SeparateOnly,
    )];
    let plan = validate(&sources, true).unwrap();
    assert_eq!(plan[0].kind, TrackKind::Clean(sources[0].id.clone()));
    assert_eq!(sources[0].route, AudioRoute::SeparateOnly);
    assert!(validate(&[], true).unwrap().is_empty());
}

#[test]
fn disconnected_and_unsupported_sources_cannot_be_admitted() {
    let mut s = source("mic", AudioSourceKind::Microphone, AudioRoute::MainMix);
    s.is_active = false;
    assert!(
        validate(&[s], true)
            .unwrap_err()
            .message
            .contains("unavailable")
    );
    let app = source(
        "app:12:a",
        AudioSourceKind::Application,
        AudioRoute::MainMix,
    );
    assert_eq!(
        validate(&[app], false).unwrap_err().code,
        ErrorCode::NotImplemented
    );
}

#[test]
fn windows_process_loopback_duplicates_are_blocked_even_when_muted() {
    let mut a = source(
        "wasapi-session:endpoint-a",
        AudioSourceKind::Application,
        AudioRoute::MainMix,
    );
    a.process_id = Some(12);
    let mut b = a.clone();
    b.id.0 = "wasapi-session:endpoint-b".into();
    b.muted = true;
    assert!(
        validate(&[a, b], true)
            .unwrap_err()
            .message
            .contains("overlapping")
    );
}

#[test]
fn pulse_streams_from_one_process_remain_independent_and_unknown_pids_do_not_alias() {
    let mut a = source(
        "pulse-app:1:1:1:12:2",
        AudioSourceKind::Application,
        AudioRoute::MainMix,
    );
    a.process_id = Some(12);
    let mut b = a.clone();
    b.id.0 = "pulse-app:1:2:1:12:2".into();
    assert!(validate(&[a, b], true).is_ok());
    let a = source(
        "wasapi-session:a",
        AudioSourceKind::Application,
        AudioRoute::MainMix,
    );
    let b = source(
        "wasapi-session:b",
        AudioSourceKind::Application,
        AudioRoute::MainMix,
    );
    assert!(!overlap(&a, &b));
}

#[test]
fn system_and_application_overlap_and_identical_ids_are_detected() {
    let system = source("system", AudioSourceKind::System, AudioRoute::MainMix);
    let app = source(
        "app",
        AudioSourceKind::Application,
        AudioRoute::MainMixAndSeparate,
    );
    assert!(validate(&[system.clone(), app], true).is_err());
    assert!(validate(&[system.clone(), system], true).is_err());
}

#[test]
fn final_batch_is_validated_and_removing_an_invalid_route_remains_possible() {
    let mut a = source(
        "wasapi-session:a",
        AudioSourceKind::Application,
        AudioRoute::MainMix,
    );
    a.process_id = Some(12);
    let mut b = a.clone();
    b.id.0 = "wasapi-session:b".into();
    b.route = AudioRoute::Ignore;
    let change = vec![
        (a.id.clone(), AudioRoute::Ignore),
        (b.id.clone(), AudioRoute::MainMix),
    ];
    assert_eq!(
        edits(&[a.clone(), b.clone()], &change, true).unwrap(),
        change
    );
    assert_eq!(
        edits(
            &[a.clone(), b.clone()],
            &[change[1].clone(), change[0].clone()],
            true
        )
        .unwrap(),
        change
    );
    assert!(
        edits(
            &[a.clone(), b.clone()],
            &[(b.id.clone(), AudioRoute::MainMix)],
            true
        )
        .is_err()
    );
    b.route = AudioRoute::MainMix;
    assert!(edits(&[a, b.clone()], &[(b.id.clone(), AudioRoute::Ignore)], true).is_ok());
    assert!(
        edits(
            &[b.clone()],
            &[
                (b.id.clone(), AudioRoute::Ignore),
                (b.id, AudioRoute::Ignore)
            ],
            true
        )
        .is_err()
    );
}

fn pcm(id: &str, start: u64, amplitude: f32) -> PcmBlock {
    PcmBlock {
        source_id: AudioSourceId(id.into()),
        timestamp: MediaTimestamp(start),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
        channels: NonZeroU16::new(2).unwrap(),
        samples: vec![amplitude; 960],
    }
}

#[test]
fn easy_separate_routes_produce_one_mix_and_independently_editable_pcm_without_double_capture() {
    let mut sources = vec![
        source("mic", AudioSourceKind::Microphone, AudioRoute::MainMix),
        source("app", AudioSourceKind::Application, AudioRoute::MainMix),
    ];
    let changes = separate_edits(&sources, true);
    assert_eq!(edits(&sources, &changes, true).unwrap(), changes);
    for (id, next) in changes {
        sources.iter_mut().find(|s| s.id == id).unwrap().route = next;
    }
    let plan = validate(&sources, true).unwrap();
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0].kind, TrackKind::MainMix);
    assert_eq!(
        plan[0].sources.len(),
        2,
        "each captured source enters the main mix exactly once"
    );
    let mut mixer = AlignedPcmProcessor::new(&selections(&sources), MediaTimestamp(0)).unwrap();
    mixer.push(pcm("mic", 0, 0.25)).map_err(|(e, _)| e).unwrap();
    mixer.push(pcm("app", 0, 0.5)).map_err(|(e, _)| e).unwrap();
    let quantum = mixer.poll(MediaTimestamp(0)).unwrap().unwrap();
    assert_eq!(quantum.tracks[0].samples, vec![0.75; 960]);
    assert_eq!(quantum.tracks[1].samples, vec![0.5; 960]);
    assert_eq!(quantum.tracks[2].samples, vec![0.25; 960]);
    assert_eq!(
        quantum.tracks[0].timestamp,
        MediaTimestamp(0),
        "no UI processing delay or timestamp shift"
    );
    mixer.set_muted(&AudioSourceId("mic".into()), true).unwrap();
    mixer
        .push(pcm("mic", 10_000_000, 0.25))
        .map_err(|(e, _)| e)
        .unwrap();
    mixer
        .push(pcm("app", 10_000_000, 0.5))
        .map_err(|(e, _)| e)
        .unwrap();
    let quantum = mixer.poll(MediaTimestamp(10_000_000)).unwrap().unwrap();
    assert_eq!(quantum.tracks[0].samples, vec![0.5; 960]);
    assert_eq!(quantum.tracks[1].samples, vec![0.5; 960]);
    assert_eq!(quantum.tracks[2].samples, vec![0.; 960]);
    assert_eq!(mixer.tracks(), plan, "mute never changes track assignments");
}
