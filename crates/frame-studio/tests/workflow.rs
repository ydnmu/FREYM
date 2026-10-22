//! Short corruption/data-loss checks for the integrated paths. Local media only.
use frame_clock::MediaTimestamp;
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
};
use frame_mux::{ContainerWriter, RecordingSession};
use frame_studio::*;
use std::{
    num::NonZeroU32,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, AtomicU64},
    time::{Duration, Instant},
};
fn root(name: &str) -> PathBuf {
    let root = PathBuf::from(
        std::env::var_os("FREYM_STUDIO_TEST_DIR").expect("Set a local evidence directory"),
    );
    let path = root.join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}
fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn decode(path: &std::path::Path, track: usize) -> Vec<u8> {
    let r = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{track}"), "-f", "f32le", "pipe:1"])
        .output()
        .unwrap();
    assert!(r.status.success());
    r.stdout
}
#[test]
#[ignore = "Requires local FFmpeg and a Studio evidence directory"]
fn mixed_colour_cursor_and_sample_rate_changes() {
    let root = root("mixed-cursor");
    let sdr = root.join("sdr.mkv");
    let hdr = root.join("hdr.mkv");
    for (path, video, rate, codec, pixel) in [
        (
            &sdr,
            "color=white:size=64x48:rate=10:duration=0.3",
            44100,
            "libx264",
            "yuv420p",
        ),
        (
            &hdr,
            "color=gray:size=80x60:rate=12:duration=0.25",
            48000,
            "ffv1",
            "yuv444p10le",
        ),
    ] {
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-v", "error", "-n", "-f", "lavfi", "-i", video, "-f", "lavfi", "-i",
        ])
        .arg(format!(
            "sine=frequency=440:sample_rate={rate}:duration={}",
            if rate == 44100 { "0.3" } else { "0.25" }
        ))
        .args([
            "-c:v",
            codec,
            "-pix_fmt",
            pixel,
            "-c:a",
            "flac",
            "-metadata:s:a:0",
            "title=main",
        ]);
        if rate == 48000 {
            cmd.args(["-vf","setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=limited","-color_primaries","bt2020","-color_trc","smpte2084","-colorspace","bt2020nc","-color_range","tv"]);
        } else {
            cmd.args([
                "-vf",
                "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=limited",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
            ]);
        }
        run(cmd.arg(path));
    }
    let before = [std::fs::read(&sdr).unwrap(), std::fs::read(&hdr).unwrap()];
    let tools = LocalTools::default();
    let (mut edit, _) = tools.open(&sdr).unwrap();
    let (added, _) = tools.open(&hdr).unwrap();
    edit.append(added).unwrap();
    for source in &mut edit.sources {
        let info = tools.probe(&source.path).unwrap();
        source.metadata = Some(InteractionMetadata {
            version: 1,
            width: info.width,
            height: info.height,
            cursor_baked: false,
            origin_ns: 0,
            sampling_interval_ns: 16_666_667,
            truncated: false,
            events: vec![
                Interaction {
                    time_ns: 0,
                    position: Point { x: 0.5, y: 0.5 },
                    kind: InteractionKind::Move,
                },
                Interaction {
                    time_ns: 100_000_000,
                    position: Point { x: 0.5, y: 0.5 },
                    kind: InteractionKind::Click,
                },
            ],
        });
    }
    edit.rebuild_metadata();
    edit.cursor.enabled = true;
    edit.cursor.smoothing_ms = 0;
    let info = tools.inspect_timeline(&edit).unwrap();
    assert_eq!(info.audio.len(), 1);
    assert_eq!(info.frames.len(), 6);
    let rejected = root.join("implicit.mkv");
    let error = tools
        .export(
            &edit,
            &rejected,
            ExportProfile::Lossless,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap_err();
    assert!(error.contains("mixes HDR/SDR"), "{error}");
    assert!(!rejected.exists());
    for (colour, transfer, profile) in [
        (
            OutputColour::HdrPq,
            "smpte2084",
            ExportProfile::Hevc { crf: 18 },
        ),
        (
            OutputColour::HdrHlg,
            "arib-std-b67",
            ExportProfile::Lossless,
        ),
        (OutputColour::Sdr, "bt709", ExportProfile::Lossless),
    ] {
        edit.output_colour = colour;
        let output = root.join(format!("{transfer}.mkv"));
        tools
            .export(
                &edit,
                &output,
                profile,
                &AtomicBool::new(false),
                &AtomicU64::new(0),
            )
            .unwrap();
        let exported = tools.probe(&output).unwrap();
        assert_eq!(exported.bit_depth, 10);
        assert_eq!(exported.frames.len(), 6);
        assert_eq!(exported.colour_transfer.as_deref(), Some(transfer));
        assert_eq!(exported.audio.len(), 1);
        assert_eq!((exported.width, exported.height), (64, 48));
        let audio = decode(&output, 0);
        assert!(
            (audio.len() / 4).abs_diff((edit.duration_ns as f64 / 1e9 * 44100.0).round() as usize)
                < 3
        );
        // Cursor white is 203 nit, independent of PQ/HLG transfer. Inspect a
        // pointer-body pixel through the reference linear-light decoder.
        if colour != OutputColour::Sdr {
            let out = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&output)
                .args([
                    "-map",
                    "0:v:0",
                    "-vf",
                    "zscale=t=linear:npl=203,format=gbrpf32le",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let pixel = 32 * 64 + 34;
            let green =
                f32::from_le_bytes(out.stdout[pixel * 4..pixel * 4 + 4].try_into().unwrap());
            assert!(
                (green - 1.0).abs() < 0.12,
                "HDR cursor reference brightness: {green}"
            );
            let background = f32::from_le_bytes(out.stdout[4 * 65..4 * 65 + 4].try_into().unwrap());
            assert!(
                (background - 1.0).abs() < 0.03,
                "SDR white normalized into HDR changed brightness: {background}"
            );
        }
    }
    edit.output_colour = OutputColour::HdrPq;
    edit.cursor.enabled = false;
    let native = root.join("mixed-native.mkv");
    tools
        .export(
            &edit,
            &native,
            ExportProfile::Lossless,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    assert_eq!(tools.probe(&native).unwrap().frames.len(), 6);
    // Planar RGB is full range even when its container omits a range tag. Both
    // preview and cursor export must accept the declared HDR gamut/transfer.
    let rgb = root.join("hdr-rgb.mkv");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-n", "-i"])
        .arg(&hdr)
        .args([
            "-vf",
            "zscale=m=gbr:r=full,format=gbrp10le",
            "-c:v",
            "ffv1",
            "-an",
        ])
        .arg(&rgb));
    let (mut rgb_edit, rgb_info) = tools.open(&rgb).unwrap();
    assert_eq!(rgb_info.pixel_format, "gbrp10le");
    assert_eq!(
        tools.decode_frame(&rgb_edit, &rgb_info, 0).unwrap().len(),
        80 * 60 * 3
    );
    rgb_edit.sources[0].metadata = edit.sources[1].metadata.clone();
    rgb_edit.rebuild_metadata();
    rgb_edit.cursor.enabled = true;
    let rgb_output = root.join("hdr-rgb-cursor.mkv");
    tools
        .export(
            &rgb_edit,
            &rgb_output,
            ExportProfile::Lossless,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    let rgb_result = tools.probe(&rgb_output).unwrap();
    assert_eq!(rgb_result.bit_depth, 10);
    assert_eq!(rgb_result.colour_transfer, rgb_info.colour_transfer);
    assert_eq!(rgb_result.colour_range.as_deref(), Some("pc"));
    edit.output_colour = OutputColour::Sdr;
    assert_eq!(std::fs::read(&sdr).unwrap(), before[0]);
    assert_eq!(std::fs::read(&hdr).unwrap(), before[1]);
    let project = root.join("edit.json");
    edit.save(&project).unwrap();
    assert_eq!(
        Timeline::load(&project).unwrap().output_colour,
        OutputColour::Sdr
    );
}
#[test]
#[ignore = "Requires local FFmpeg and a Studio evidence directory"]
fn multisegment_trim_split_order_audio_and_playback() {
    let root = root("multisegment");
    let a = root.join("a.mkv");
    let b = root.join("b.mkv");
    for (path, frequency) in [(&a, 440), (&b, 880)] {
        run(Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-n",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10:duration=0.4",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency={frequency}:sample_rate=48000:duration=0.4"),
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "1:a",
                "-c:v",
                "libx264",
                "-c:a",
                "flac",
                "-metadata:s:a:0",
                "title=main",
                "-metadata:s:a:1",
                "title=mic",
            ])
            .arg(path));
    }
    let tools = LocalTools::default();
    let missing = LocalTools {
        ffmpeg: root.join("missing-ffmpeg.exe"),
        ffprobe: tools.ffprobe.clone(),
    };
    let error = missing.capabilities().unwrap_err();
    assert!(error.contains("FREYM_FFMPEG") && error.contains("missing-ffmpeg"));
    let (mut edit, _) = tools.open(&a).unwrap();
    let binfo = tools.probe(&b).unwrap();
    edit.sources.push(SourceSegment {
        path: std::fs::canonicalize(&b).unwrap(),
        identity: SourceIdentity::read(&b).unwrap(),
        origin_ns: 600_000_000,
        duration_ns: binfo.duration_ns,
        metadata: None,
    });
    edit.clips.push(TimelineClip {
        source_index: 1,
        in_ns: 0,
        out_ns: binfo.duration_ns,
    });
    edit.duration_ns += binfo.duration_ns;
    let before = edit.clone();
    let cached = tools.inspect_timeline(&before).unwrap();
    edit.split(200_000_000).unwrap();
    edit.trim(0, 100_000_000, 200_000_000).unwrap();
    edit.move_clip(2, 0).unwrap();
    let json = sidecar_path(&a, ".freym-edit.json");
    edit.save(&json).unwrap();
    let edit = Timeline::load(&json).unwrap();
    let info = tools.inspect_timeline(&edit).unwrap();
    // UI edits reuse verified source facts; timestamps and stems must match a
    // fresh inspection exactly after split, trim and reordering.
    let retimed = tools.retime_timeline(&edit, &before, &cached).unwrap();
    assert_eq!(retimed.frames, info.frames);
    assert_eq!(retimed.audio, info.audio);
    assert_eq!(retimed.duration_ns, info.duration_ns);
    assert_eq!(info.frames.len(), 7);
    assert_eq!(info.audio_tracks, 2);
    let originals = [std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap()];
    let out = root.join("edited.mkv");
    tools
        .export(
            &edit,
            &out,
            ExportProfile::Lossless,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    assert_eq!(tools.probe(&out).unwrap().audio_tracks, 2);
    let a_pcm = decode(&a, 0);
    let b_pcm = decode(&b, 0);
    // Source samples must follow the exact edit order, with neither track mixed.
    let mut expected = b_pcm.clone();
    expected.extend_from_slice(&a_pcm[4800 * 4..9600 * 4]);
    expected.extend_from_slice(&a_pcm[9600 * 4..]);
    assert_eq!(decode(&out, 0), expected);
    assert_eq!(decode(&out, 1), expected);
    for (name, profile) in [
        ("h264", ExportProfile::H264 { crf: 18 }),
        ("hevc", ExportProfile::Hevc { crf: 18 }),
        ("av1", ExportProfile::Av1 { crf: 24 }),
    ] {
        let output = root.join(format!("sdr-{name}.mkv"));
        tools
            .export(
                &edit,
                &output,
                profile,
                &AtomicBool::new(false),
                &AtomicU64::new(0),
            )
            .unwrap();
        let exported = tools.probe(&output).unwrap();
        assert_eq!(exported.frames.len(), info.frames.len());
        assert_eq!(exported.audio_tracks, 2);
        assert_eq!(exported.colour_space.as_deref(), Some("bt709"));
        assert_eq!(exported.colour_range.as_deref(), Some("tv"));
        run(Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&output)
            .args(["-map", "0", "-f", "null", "-"]));
    }
    assert_eq!(std::fs::read(&a).unwrap(), originals[0]);
    assert_eq!(std::fs::read(&b).unwrap(), originals[1]);
    let player = Playback::start(tools.clone(), edit.clone(), info.clone(), 0, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frames = 0;
    while !player.finished() && Instant::now() < deadline {
        if player.take_frame().is_some() {
            frames += 1;
        }
        if frames > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(frames > 0, "{:?}", player.take_error());
    player.pause(true);
    std::thread::sleep(Duration::from_millis(30));
    let paused = player.time_ns();
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(player.time_ns(), paused);
    player.pause(false);
    player.stop();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !player.finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(player.finished());
    // Exercise device audio without requiring it on machines without an output.
    if std::env::var_os("FREYM_STUDIO_AUDIO_CHECK").is_some() {
        let player = Playback::start(tools.clone(), edit, info, 100_000_000, Some(1));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !player.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(player.finished(), "audio did not drain");
        assert!(player.take_error().is_none());
        assert!(player.time_ns() >= 600_000_000);
    }
    // Version 1 JSON still opens without segment fields. Unknown future versions
    // are rejected before playback/export; sources are never rewritten.
    let mut old = serde_json::to_value(tools.open(&b).unwrap().0).unwrap();
    old["version"] = 1.into();
    old["source"] = serde_json::to_value(std::fs::canonicalize(&b).unwrap()).unwrap();
    for key in ["sources", "clips", "warnings"] {
        old.as_object_mut().unwrap().remove(key);
    }
    let old_path = root.join("legacy.freym-edit.json");
    std::fs::write(&old_path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(Timeline::load(&old_path).unwrap().version, 1);
    old["version"] = 3.into();
    std::fs::write(&old_path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert!(Timeline::load(&old_path).is_err());
}
#[test]
#[ignore = "Requires local HEVC/AV1 FFmpeg encoders"]
fn manifest_recovery_and_preserving_hdr_exports() {
    let root = root("session-hdr");
    let manifest = root.join("recording.mkv.freym-session");
    let mut session = RecordingSession::create(&manifest).unwrap();
    let data = include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.hevc");
    let nz = |v| NonZeroU32::new(v).unwrap();
    let mut stream = EncodedStreamInfo {
        capability: EncoderCapability {
            id: "fixture".into(),
            codec: "hevc".into(),
            hardware: false,
        },
        width: nz(64),
        height: nz(64),
        nominal_rate_num: nz(10),
        nominal_rate_den: nz(1),
        bitstream_format: BitstreamFormat::HevcAnnexB,
        colour: Some(VideoColour::BT2020_PQ_LIMITED),
        codec_config: data.to_vec(),
        allows_reordering: false,
    };
    for (i, origin) in [7_000_000_000u64, 9_000_000_000].into_iter().enumerate() {
        let mut writer = session
            .create_video_segment(&stream, MediaTimestamp(origin))
            .unwrap();
        for frame in 0..10 {
            let pts = origin + frame * 100_000_000;
            writer
                .write(EncodedPacket {
                    track_id: 1,
                    pts: MediaTimestamp(pts),
                    dts: MediaTimestamp(pts),
                    duration_ns: 100_000_000,
                    keyframe: true,
                    data: data.to_vec(),
                })
                .unwrap();
        }
        if i == 0 {
            writer.finalize().unwrap();
        } else {
            writer.checkpoint().unwrap();
            drop(writer);
        }
    }
    let original = session.snapshot().unwrap();
    drop(session);
    let source = root.join(&original.segments[0].file_name);
    let bytes = std::fs::read(&source).unwrap();
    let tools = LocalTools::default();
    let (mut edit, info) = tools.open(&source).unwrap();
    assert_eq!(edit.sources.len(), 2);
    assert!(!edit.warnings.is_empty());
    assert_eq!(info.bit_depth, 10);
    assert!(info.hdr());
    assert_eq!(edit.sources[0].origin_ns, 7_000_000_000);
    assert_eq!(edit.clips[0].duration_ns(), 2_000_000_000);
    // Resolve a later recovered segment back to its manifest.
    let (resolved, _) = tools.open(&edit.sources[1].path).unwrap();
    assert_eq!(resolved.sources.len(), 2);
    let native = root.join("native-ffv1.mkv");
    tools
        .export(
            &edit,
            &native,
            ExportProfile::Lossless,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    let yuv = |path: &std::path::Path| {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(path)
            .args([
                "-map",
                "0:v",
                "-pix_fmt",
                "yuv420p10le",
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    };
    let mut expected = yuv(&edit.sources[0].path);
    expected.extend(yuv(&edit.sources[1].path));
    assert_eq!(
        yuv(&native),
        expected,
        "native 10-bit sample precision changed"
    );
    edit.zooms.push(ZoomSegment {
        enabled: true,
        start_ns: 0,
        end_ns: 500_000_000,
        ramp_ns: 100_000_000,
        scale: 2.,
        focus: Point { x: 0.6, y: 0.4 },
        easing: Easing::Smooth,
    });
    for (name, profile) in [
        ("hevc", ExportProfile::Hevc { crf: 18 }),
        ("av1", ExportProfile::Av1 { crf: 24 }),
        ("ffv1", ExportProfile::Lossless),
    ] {
        let output = root.join(format!("{name}.mkv"));
        tools
            .export(
                &edit,
                &output,
                profile,
                &AtomicBool::new(false),
                &AtomicU64::new(0),
            )
            .unwrap();
        let result = tools.probe(&output).unwrap();
        assert_eq!(result.bit_depth, 10);
        assert_eq!(result.colour_transfer, info.colour_transfer);
        assert_eq!(result.colour_range, info.colour_range);
        assert_eq!(result.colour_space, info.colour_space);
        run(Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&output)
            .args(["-f", "null", "-"]));
    }
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert!(
        tools
            .export(
                &edit,
                &root.join("bad-h264.mkv"),
                ExportProfile::default(),
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
    );
    assert!(!root.join("bad-h264.mkv").exists());
    let supplied = root.join("supplied.mkv.freym-session");
    let mut session = RecordingSession::create(&supplied).unwrap();
    let colour = stream.colour.as_mut().unwrap();
    colour.content_light = Some(frame_encode::ContentLight {
        max_cll: 1000,
        max_fall: 400,
    });
    colour.mastering = Some(frame_encode::MasteringDisplay {
        red: (0.68, 0.32),
        green: (0.265, 0.69),
        blue: (0.15, 0.06),
        white: (0.3127, 0.329),
        luminance_max: 1000.,
        luminance_min: 0.005,
    });
    let mut writer = session
        .create_video_segment(&stream, MediaTimestamp(0))
        .unwrap();
    for frame in 0..10 {
        let pts = frame * 100_000_000;
        writer
            .write(EncodedPacket {
                track_id: 1,
                pts: MediaTimestamp(pts),
                dts: MediaTimestamp(pts),
                duration_ns: 100_000_000,
                keyframe: true,
                data: data.to_vec(),
            })
            .unwrap();
    }
    writer.finalize().unwrap();
    session.finish().unwrap();
    drop(session);
    let (edit, info) = tools.open(&supplied).unwrap();
    assert_eq!(info.hdr_metadata.len(), 2);
    let mut legacy_hdr = edit.clone();
    legacy_hdr.version = 1;
    legacy_hdr.source = legacy_hdr.sources[0].path.clone();
    legacy_hdr.identity = legacy_hdr.sources[0].identity.clone();
    legacy_hdr.sources.clear();
    legacy_hdr.clips.clear();
    let legacy_output = root.join("legacy-hdr-h264.mkv");
    assert!(
        tools
            .export(
                &legacy_hdr,
                &legacy_output,
                ExportProfile::default(),
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
    );
    assert!(!legacy_output.exists());
    for (name, profile) in [
        ("mastered-hevc", ExportProfile::Hevc { crf: 18 }),
        ("mastered-av1", ExportProfile::Av1 { crf: 24 }),
    ] {
        let output = root.join(format!("{name}.mkv"));
        tools
            .export(
                &edit,
                &output,
                profile,
                &AtomicBool::new(false),
                &AtomicU64::new(0),
            )
            .unwrap();
        assert_eq!(
            tools.probe(&output).unwrap().hdr_metadata,
            info.hdr_metadata
        );
    }
    // Replay gets an independent valid-prefix metadata sidecar. Deleting the
    // original event file after saving cannot change the clip's editing events.
    let events = root.join("events.jsonl");
    let sidecar = root.join("events.freym-events");
    let records = [
        frame_metadata::Record::Header(frame_metadata::Header {
            version: 1,
            origin_ns: 0,
            cursor_baked: Some(false),
            capabilities: Default::default(),
        }),
        frame_metadata::Record::Event(frame_metadata::Event {
            version: 1,
            sequence: 1,
            raw_session_ns: 0,
            session_ns: 0,
            event: frame_metadata::EventKind::Geometry {
                bounds: frame_metadata::Geometry {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
            },
        }),
        frame_metadata::Record::Event(frame_metadata::Event {
            version: 1,
            sequence: 2,
            raw_session_ns: 200_000_000,
            session_ns: 200_000_000,
            event: frame_metadata::EventKind::Button {
                x: 32,
                y: 32,
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
        &events,
        records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    frame_metadata::import_jsonl(&events, &sidecar).unwrap();
    let carried = read_recording_metadata(&sidecar, 500_000_000, 64, 64, true).unwrap();
    assert_eq!(carried.events.len(), 1);
    assert_eq!(carried.events[0].time_ns, 0);
    assert_eq!(carried.events[0].kind, InteractionKind::Move);
    assert_eq!(carried.events[0].position, Point { x: 0.5, y: 0.5 });
    let mut replay = frame_mux::replay::ReplayBuffer::new(
        stream,
        vec![],
        frame_mux::replay::ReplayLimit {
            duration_ns: 1_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    for frame in 0..10 {
        let pts = frame * 100_000_000;
        replay.push_video(&EncodedPacket {
            track_id: 1,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: 100_000_000,
            keyframe: true,
            data: data.to_vec(),
        });
    }
    let clip = root.join("replay.mkv");
    replay
        .save_session_with_metadata(&clip, 1_000_000_000, Some(&sidecar))
        .unwrap();
    std::fs::remove_file(sidecar).unwrap();
    let (edit, _) = tools.open(&clip).unwrap();
    assert!(
        edit.metadata
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == InteractionKind::Click)
    );
}
