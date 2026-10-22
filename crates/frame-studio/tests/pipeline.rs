use frame_clock::MediaTimestamp;
use frame_studio::*;
use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, AtomicU64},
};
fn timeline() -> Timeline {
    Timeline {
        version: 1,
        source: PathBuf::new(),
        identity: SourceIdentity {
            bytes: 0,
            modified_ns: 0,
        },
        duration_ns: 3_000_000_000,
        metadata: Some(InteractionMetadata {
            version: 1,
            width: 320,
            height: 180,
            cursor_baked: false,
            origin_ns: 0,
            sampling_interval_ns: 0,
            truncated: false,
            events: vec![
                Interaction {
                    time_ns: 800_000_000,
                    position: Point { x: 0.9, y: 0.1 },
                    kind: InteractionKind::Click,
                },
                Interaction {
                    time_ns: 900_000_000,
                    position: Point { x: 0.8, y: 0.2 },
                    kind: InteractionKind::Move,
                },
            ],
        }),
        zooms: vec![],
        cursor: CursorEffects::default(),
        sources: vec![],
        clips: vec![],
        warnings: vec![],
        output_colour: OutputColour::default(),
    }
}
#[test]
fn zoom_suggestions_interpolate_and_disable_without_mutating_events() {
    let mut edit = timeline();
    edit.suggest_zooms(2.0);
    edit.validate().unwrap();
    assert_eq!(edit.zooms.len(), 1);
    assert_eq!(edit.metadata.as_ref().unwrap().events.len(), 2);
    let zoom = &edit.zooms[0];
    let start = zoom.start_ns;
    let end = zoom.end_ns;
    assert_eq!(edit.viewport(start).width, 1.0);
    assert_eq!(edit.viewport(end).width, 1.0);
    let center = edit.viewport(800_000_000);
    assert_eq!(center.width, 0.5);
    assert!(center.left + center.width <= 1.0);
    let near = edit.viewport(start + 1_000);
    assert!((near.width - 1.0).abs() < 1e-10);
    edit.zooms[0].enabled = false;
    assert_eq!(edit.viewport(800_000_000).width, 1.0);
    edit.zooms[0].scale = f64::NAN;
    assert!(edit.validate().is_err());
}
#[test]
fn smoothing_emphasis_and_zoom_render_change_pixels_deterministically() {
    let mut edit = timeline();
    let raw = vec![90u8; 320 * 180 * 3];
    assert_eq!(
        render_rgb(&edit, MediaTimestamp(900_000_000), 320, 180, &raw),
        raw
    );
    edit.cursor.enabled = true;
    let (smoothed, clicked) = edit.cursor_at(900_000_000).unwrap();
    assert!(smoothed.x > 0.8 && smoothed.x < 0.9);
    assert!(clicked);
    let rendered = render_rgb(&edit, MediaTimestamp(900_000_000), 320, 180, &raw);
    assert_ne!(rendered, raw);
    assert_eq!(
        rendered,
        render_rgb(&edit, MediaTimestamp(900_000_000), 320, 180, &raw)
    );
    edit.metadata.as_mut().unwrap().cursor_baked = true;
    assert!(edit.validate().is_err());
    edit.cursor.smoothing_ms = 0;
    edit.validate().unwrap();
}
#[test]
fn recording_sidecar_maps_segment_origin_geometry_and_loss() {
    let directory = std::env::var_os("FREYM_STUDIO_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("freym-studio-tests"));
    std::fs::create_dir_all(&directory).unwrap();
    let input = directory.join(format!("metadata-{}.jsonl", std::process::id()));
    let sidecar = directory.join(format!("metadata-{}.freym-events", std::process::id()));
    let events = [
        frame_metadata::Record::Header(frame_metadata::Header {
            version: 1,
            origin_ns: 100,
            cursor_baked: Some(false),
            capabilities: Default::default(),
        }),
        frame_metadata::Record::Event(frame_metadata::Event {
            version: 1,
            sequence: 1,
            raw_session_ns: 100,
            session_ns: 100,
            event: frame_metadata::EventKind::Geometry {
                bounds: frame_metadata::Geometry {
                    x: -320,
                    y: 0,
                    width: 320,
                    height: 180,
                },
            },
        }),
        frame_metadata::Record::Event(frame_metadata::Event {
            version: 1,
            sequence: 2,
            raw_session_ns: 200,
            session_ns: 200,
            event: frame_metadata::EventKind::Button {
                x: 160,
                y: 90,
                button: frame_metadata::Button::Left,
                pressed: true,
            },
        }),
        frame_metadata::Record::End {
            completed: true,
            events: 2,
            dropped: 0,
        },
    ];
    std::fs::write(
        &input,
        events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    frame_metadata::import_jsonl(&input, &sidecar).unwrap();
    let metadata = read_recording_metadata(&sidecar, 150, 320, 180, false).unwrap();
    assert_eq!(metadata.events[0].time_ns, 50);
    assert_eq!(metadata.events[0].position, Point { x: 0.5, y: 0.5 });
    assert!(!metadata.truncated);
}
#[test]
#[ignore = "Requires local FFmpeg; run with FREYM_STUDIO_TEST_DIR under C:/FREYM/build."]
fn rendered_export_preserves_vfr_audio_and_originals() {
    let root =
        PathBuf::from(std::env::var_os("FREYM_STUDIO_TEST_DIR").expect("evidence directory"));
    std::fs::create_dir_all(&root).unwrap();
    let raster = root.join("input-pictures.mkv");
    let source = root.join("source.mkv");
    let output = root.join("rendered.mkv");
    let times = [
        (30_000_000, 40_000_000),
        (70_000_000, 60_000_000),
        (130_000_000, 100_000_000),
        (230_000_000, 40_000_000),
    ];
    let mut writer = frame_mux::MkvRasterWriter::create(&raster, 64, 48).unwrap();
    for (i, (pts, duration)) in times.iter().enumerate() {
        let rgb: Vec<u8> = (0..64 * 48)
            .flat_map(|p| [(p % 64 * 4) as u8, (p / 64 * 5) as u8, (i * 50) as u8])
            .collect();
        writer
            .write(
                MediaTimestamp(*pts),
                *duration,
                &encode_png(64, 48, &rgb).unwrap(),
            )
            .unwrap();
    }
    writer.finish().unwrap();
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-n", "-copyts", "-i"])
        .arg(&raster)
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=0.27",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "1:a",
            "-metadata:s:a:0",
            "title=main",
            "-metadata:s:a:1",
            "title=isolated",
            "-c:v",
            "libx264",
            "-crf",
            "0",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-pix_fmt",
            "yuv444p",
            "-fps_mode",
            "passthrough",
            "-c:a",
            "libopus",
            "-f",
            "matroska",
        ])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    let original = std::fs::read(&source).unwrap();
    let tools = LocalTools::default();
    let (mut edit, info) = tools.open(&source).unwrap();
    // Exercise the retained version 1 export route as well as v2 workflow tests.
    edit.version = 1;
    edit.sources.clear();
    edit.clips.clear();
    assert_eq!(info.frames.len(), 4);
    edit.zooms.push(ZoomSegment {
        enabled: true,
        start_ns: 0,
        end_ns: edit.duration_ns,
        ramp_ns: 40_000_000,
        scale: 2.0,
        focus: Point { x: 0.7, y: 0.5 },
        easing: Easing::Smooth,
    });
    let path = sidecar_path(&source, ".freym-edit.json");
    edit.save(&path).unwrap();
    assert_eq!(Timeline::load(&path).unwrap().zooms.len(), 1);
    let mut video_duration_project = edit.clone();
    video_duration_project.zooms.clear();
    let (last_pts, last_duration) = info.frames.last().unwrap();
    video_duration_project.duration_ns = last_pts.0 + last_duration;
    let old_path = root.join("legacy-video-clock.freym-edit.json");
    video_duration_project.save(&old_path).unwrap();
    assert_eq!(
        tools.open(&old_path).unwrap().1.duration_ns,
        video_duration_project.duration_ns
    );
    let report = tools
        .export(
            &edit,
            &output,
            ExportProfile::Lossless,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    assert_eq!(report.frames, 4);
    assert_eq!(report.audio_tracks, 2);
    let exported = tools.probe(&output).unwrap();
    assert_eq!(exported.colour_primaries, info.colour_primaries);
    assert_eq!(exported.colour_transfer, info.colour_transfer);
    assert_eq!(info.frames.len(), exported.frames.len());
    for ((a, _), (b, _)) in info.frames.iter().zip(&exported.frames) {
        assert!(a.0.abs_diff(b.0) <= 1_000_000, "{a:?} != {b:?}");
    }
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert!(
        tools
            .export(
                &edit,
                &output,
                ExportProfile::Lossless,
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
    );
    let decode = |path: &std::path::Path| {
        Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-f", "s16le", "pipe:1"])
            .output()
            .unwrap()
            .stdout
    };
    assert_eq!(decode(&source), decode(&output));
    let raw = tools
        .decode_frame(
            &Timeline {
                zooms: vec![],
                ..edit.clone()
            },
            &info,
            100_000_000,
        )
        .unwrap();
    let changed = tools.decode_frame(&edit, &info, 100_000_000).unwrap();
    assert_ne!(raw, changed);
    let decoded_export = tools
        .decode_frame(
            &Timeline {
                version: 1,
                sources: vec![],
                clips: vec![],
                source: output.clone(),
                identity: SourceIdentity::read(&output).unwrap(),
                zooms: vec![],
                ..edit.clone()
            },
            &exported,
            100_000_000,
        )
        .unwrap();
    assert_eq!(changed, decoded_export);
    // The default H.264 route must preserve timestamps and both encoded audio
    // tracks, including the source's nonzero first-video offset and Opus trim.
    let compressed = root.join("h264.mkv");
    tools
        .export(
            &edit,
            &compressed,
            ExportProfile::default(),
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    let compressed_info = tools.probe(&compressed).unwrap();
    assert_eq!(compressed_info.colour_primaries, info.colour_primaries);
    assert_eq!(compressed_info.colour_transfer, info.colour_transfer);
    assert_eq!(compressed_info.frames.len(), info.frames.len());
    assert_eq!(compressed_info.audio_tracks, 2);
    for ((a, d), (b, e)) in info.frames.iter().zip(&compressed_info.frames) {
        assert!(a.0.abs_diff(b.0) <= 1_000_000);
        assert!(d.abs_diff(*e) <= 1_000_000);
    }
    assert_eq!(decode(&source), decode(&compressed));
    let packets = |path: &std::path::Path| {
        let result = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_packets",
                "-show_data_hash",
                "sha256",
                "-show_entries",
                "packet=stream_index,pts_time,duration_time,data_hash,side_data_list",
                "-of",
                "json",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(result.status.success());
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()["packets"].clone()
    };
    assert_eq!(packets(&source), packets(&output));
    assert_eq!(packets(&source), packets(&compressed));
    let cancelled = root.join("cancelled.mkv");
    assert!(
        tools
            .export(
                &edit,
                &cancelled,
                ExportProfile::Lossless,
                &AtomicBool::new(true),
                &AtomicU64::new(0)
            )
            .is_err()
    );
    assert!(!cancelled.exists());
}
