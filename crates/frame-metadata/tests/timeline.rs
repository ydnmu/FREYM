use frame_clock::{MasterClock, PauseTimeline};
use frame_metadata::*;
use std::{path::PathBuf, sync::Arc, time::Duration};

fn directory(name: &str) -> PathBuf {
    let root = std::env::var_os("FREYM_METADATA_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("freym-metadata-checks"));
    let root = root.join(format!(
        "{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn pause_alignment_interchange_and_interrupted_prefix() {
    let root = directory("timeline");
    let path = root.join("events.freym-events");
    let clock = Arc::new(MasterClock::default());
    let mut recorder = MetadataRecorder::start(
        &path,
        clock.clone(),
        PauseTimeline::default(),
        Header {
            version: SCHEMA_VERSION,
            origin_ns: 0,
            cursor_baked: None,
            capabilities: Capabilities::default(),
        },
        MetadataOptions::default(),
    )
    .unwrap();
    recorder.lifecycle(Lifecycle::Started).unwrap();
    let ingress = recorder.ingress();
    assert!(ingress.try_emit(EventKind::Geometry {
        bounds: Geometry {
            x: -100,
            y: 20,
            width: 800,
            height: 600
        }
    }));
    assert!(ingress.try_emit(EventKind::Cursor { x: 5, y: 10 }));
    let mut media = PauseTimeline::default();
    recorder.transition(&mut media, Lifecycle::Paused).unwrap();
    let pause = media.map(clock.now()).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    assert!(!ingress.try_emit(EventKind::Cursor { x: 100, y: 100 }));
    recorder.transition(&mut media, Lifecycle::Resumed).unwrap();

    assert!(ingress.try_emit(EventKind::Button {
        x: 9,
        y: 2,
        button: Button::Left,
        pressed: true
    }));
    assert!(ingress.try_emit(EventKind::Scroll {
        x: 9,
        y: 2,
        horizontal: false,
        delta: -120
    }));
    recorder.finish(true).unwrap();
    drop(recorder);
    assert!(!ingress.try_emit(EventKind::Cursor { x: 0, y: 0 }));
    let mut records = Vec::new();
    let summary = read_sidecar(&path, |record| {
        records.push(record.clone());
        Ok(())
    })
    .unwrap();
    assert!(summary.completed && summary.clean_end);
    assert_eq!(summary.events, 8);
    let paused: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Event(Event {
                session_ns,
                event:
                    EventKind::Lifecycle {
                        state: Lifecycle::Paused | Lifecycle::Resumed,
                    },
                ..
            }) => Some(*session_ns),
            _ => None,
        })
        .collect();
    assert_eq!(paused, vec![pause.0, pause.0]);
    let json = root.join("export.jsonl");
    export_jsonl(&path, &json).unwrap();
    let imported = root.join("import.freym-events");
    assert_eq!(import_jsonl(&json, &imported).unwrap().events, 8);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(&imported).unwrap()
    );
    // CRC corruption or truncation after a valid End cannot be laundered as completion.
    let mut original = std::fs::read(&path).unwrap();
    original.extend_from_slice(b"partial");
    let broken = root.join("broken.freym-events");
    std::fs::write(&broken, &original).unwrap();
    assert!(read_sidecar(&broken, |_| Ok(())).is_err());
    let recovered = root.join("recovered.freym-events");
    let restored = recover_sidecar(&broken, &recovered).unwrap();
    assert!(!restored.clean_end && !restored.completed);
    assert_eq!(restored.events, 8);
    assert!(!read_sidecar(&recovered, |_| Ok(())).unwrap().completed);
    assert_eq!(std::fs::read(&broken).unwrap(), original);
    // Untrusted JSONL cannot introduce text payloads or paths into the event schema.
    let text = std::fs::read_to_string(&json).unwrap().replace(
        "\"kind\":\"cursor\",",
        "\"kind\":\"cursor\",\"typed_text\":\"secret\",",
    );
    let invalid = root.join("invalid.jsonl");
    std::fs::write(&invalid, text).unwrap();
    let refused = root.join("refused.freym-events");
    assert!(import_jsonl(&invalid, &refused).is_err());
    assert!(!refused.exists());
    assert!(import_jsonl(&json, &path).is_err());
}
