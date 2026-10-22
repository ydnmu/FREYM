//! Short, real encoded fixtures for packet preservation and safe hybrid fallback.
use frame_studio::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, AtomicU64},
};

fn run(args: &[&str], output: &Path) {
    let result = Command::new(LocalTools::default().ffmpeg)
        .args(["-v", "error", "-xerror", "-nostdin", "-n", "-threads", "2"])
        .args(args)
        .arg(output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn root(name: &str) -> PathBuf {
    let root =
        PathBuf::from(std::env::var_os("FREYM_STUDIO_TEST_DIR").expect("Set evidence directory"))
            .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}
fn export(
    tools: &LocalTools,
    edit: &Timeline,
    path: &Path,
    profile: ExportProfile,
) -> ExportReport {
    tools
        .export(
            edit,
            path,
            profile,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap()
}
fn hashes(tools: &LocalTools, path: &Path, stream: &str) -> Vec<String> {
    let r = Command::new(&tools.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            stream,
            "-show_packets",
            "-show_data_hash",
            "sha256",
            "-show_entries",
            "packet=data_hash",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(r.status.success());
    let data: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
    data["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["data_hash"].as_str().unwrap().to_owned())
        .collect()
}
fn pcm(tools: &LocalTools, path: &Path, track: usize) -> Vec<u8> {
    let r = Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{track}"), "-f", "s16le", "pipe:1"])
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    r.stdout
}
const COLOUR: &[&str] = &[
    "-color_primaries",
    "bt709",
    "-color_trc",
    "bt709",
    "-colorspace",
    "bt709",
    "-color_range",
    "tv",
];
fn h264(path: &Path, preset: &str, bframes: &str) {
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=64x48:rate=10:duration=3",
        "-c:v",
        "libx264",
        "-preset",
        preset,
        "-crf",
        "18",
        "-g",
        "10",
        "-bf",
        bframes,
        "-threads:v",
        "2",
        "-pix_fmt",
        "yuv420p",
    ];
    args.extend_from_slice(COLOUR);
    run(&args, path);
}
#[test]
#[ignore = "Requires local FFmpeg; run through the owned task runner with FREYM_STUDIO_TEST_DIR"]
fn packet_copy_boundary_render_and_incompatible_encoder_fallback() {
    let root = root("h264");
    let source = root.join("source.mkv");
    h264(&source, "medium", "0");
    let bytes = std::fs::read(&source).unwrap();
    let tools = LocalTools::default();
    let (mut edit, _) = tools.open(&source).unwrap();
    edit.trim(0, 1_000_000_000, 2_000_000_000).unwrap();
    let copy = root.join("copy.mkv");
    let report = export(&tools, &edit, &copy, ExportProfile::default());
    assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
    assert_eq!(report.copied_video_frames, 10);
    assert_eq!(
        hashes(&tools, &copy, "v:0"),
        hashes(&tools, &source, "v:0")[10..20]
    );

    let (mut edit, _) = tools.open(&source).unwrap();
    edit.trim(0, 300_000_000, 2_600_000_000).unwrap();
    let smart = root.join("smart.mkv");
    let report = export(&tools, &edit, &smart, ExportProfile::default());
    assert_eq!(report.method, ExportMethod::SmartRender, "{report:?}");
    assert_eq!(
        (report.copied_video_frames, report.reencoded_video_frames),
        (16, 7)
    );
    assert_eq!(
        &hashes(&tools, &smart, "v:0")[7..],
        &hashes(&tools, &source, "v:0")[10..26]
    );
    assert_eq!(tools.probe(&smart).unwrap().duration_ns, 2_300_000_000);
    // Splitting adjacent source ranges does not manufacture an encoded boundary.
    let (mut split, _) = tools.open(&source).unwrap();
    split.split(350_000_000).unwrap();
    let report = export(
        &tools,
        &split,
        &root.join("split.mkv"),
        ExportProfile::default(),
    );
    assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
    assert_eq!(split.clips.len(), 2);

    let (mut joined, _) = tools.open(&source).unwrap();
    joined.trim(0, 0, 1_000_000_000).unwrap();
    let (mut later, _) = tools.open(&source).unwrap();
    later.trim(0, 2_000_000_000, 3_000_000_000).unwrap();
    joined.append(later).unwrap();
    joined.move_clip(1, 0).unwrap();
    let json = root.join("joined.freym-edit.json");
    joined.save(&json).unwrap();
    let json_bytes = std::fs::read(&json).unwrap();
    let joined = Timeline::load(&json).unwrap();
    let output = root.join("joined.mkv");
    let report = export(&tools, &joined, &output, ExportProfile::default());
    assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
    let source_hashes = hashes(&tools, &source, "v:0");
    let mut ordered = source_hashes[20..30].to_vec();
    ordered.extend_from_slice(&source_hashes[..10]);
    assert_eq!(hashes(&tools, &output, "v:0"), ordered);
    assert_eq!(std::fs::read(json).unwrap(), json_bytes);

    let (mut effects, _) = tools.open(&source).unwrap();
    effects.zooms.push(ZoomSegment {
        enabled: true,
        start_ns: 0,
        end_ns: 1_000_000_000,
        ramp_ns: 100_000_000,
        scale: 1.5,
        focus: Point { x: 0.5, y: 0.5 },
        easing: Easing::Linear,
    });
    let report = export(
        &tools,
        &effects,
        &root.join("effects-full.mkv"),
        ExportProfile::default(),
    );
    assert_eq!(report.method, ExportMethod::FullRender);

    let reordered = root.join("bframes.mkv");
    h264(&reordered, "medium", "2");
    let (whole, _) = tools.open(&reordered).unwrap();
    let report = export(
        &tools,
        &whole,
        &root.join("bframes-whole.mkv"),
        ExportProfile::default(),
    );
    assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
    let (mut edit, _) = tools.open(&reordered).unwrap();
    edit.trim(0, 1_000_000_000, 2_000_000_000).unwrap();
    let output = root.join("bframes-copy.mkv");
    let report = export(&tools, &edit, &output, ExportProfile::default());
    assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
    assert_eq!(
        hashes(&tools, &output, "v:0"),
        hashes(&tools, &reordered, "v:0")[10..20]
    );
    let (mut edit, _) = tools.open(&reordered).unwrap();
    edit.trim(0, 300_000_000, 2_600_000_000).unwrap();
    let report = export(
        &tools,
        &edit,
        &root.join("bframes-full.mkv"),
        ExportProfile::default(),
    );
    assert_eq!(report.method, ExportMethod::FullRender, "{report:?}");
    assert!(report.fallback_reason.is_some());

    let delayed = root.join("delayed-picture.mkv");
    run(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=10:duration=0.5",
            "-vf",
            "settb=1/1000000,setpts=PTS+30000",
            "-enc_time_base:v",
            "1:1000000",
            "-c:v",
            "libx264",
            "-bf",
            "0",
            "-pix_fmt",
            "yuv420p",
        ],
        &delayed,
    );
    let (edit, original_info) = tools.open(&delayed).unwrap();
    assert_eq!(original_info.frames[0].0.0, 30_000_000);
    let output = root.join("delayed-copy.mkv");
    let report = export(&tools, &edit, &output, ExportProfile::default());
    assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
    assert_eq!(tools.probe(&output).unwrap().frames, original_info.frames);

    let incompatible = root.join("ultrafast.mkv");
    h264(&incompatible, "ultrafast", "0");
    let (mut edit, _) = tools.open(&incompatible).unwrap();
    edit.trim(0, 300_000_000, 2_600_000_000).unwrap();
    let report = export(
        &tools,
        &edit,
        &root.join("incompatible-full.mkv"),
        ExportProfile::default(),
    );
    assert_eq!(report.method, ExportMethod::FullRender, "{report:?}");
    assert!(
        report
            .fallback_reason
            .as_deref()
            .unwrap()
            .contains("extradata")
    );
    let cancel = AtomicBool::new(true);
    let cancelled = root.join("cancelled.mkv");
    assert!(
        tools
            .export(
                &edit,
                &cancelled,
                ExportProfile::default(),
                &cancel,
                &AtomicU64::new(0)
            )
            .is_err()
    );
    assert!(!cancelled.exists());
    assert!(
        tools
            .export(
                &edit,
                &copy,
                ExportProfile::default(),
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
    );
    assert_eq!(std::fs::read(source).unwrap(), bytes);
}

#[test]
#[ignore = "Requires local FFmpeg; run through the owned task runner with FREYM_STUDIO_TEST_DIR"]
fn independent_audio_packet_cuts_and_sample_accurate_opus_fallback() {
    let root = root("audio");
    let tools = LocalTools::default();
    for codec in ["pcm_s16le", "libopus"] {
        let source = root.join(format!("{codec}.mkv"));
        let mut args = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=10:duration=3.2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3.2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=3.2",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-g",
            "16",
            "-bf",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            codec,
            "-metadata:s:a:0",
            "title=microphone",
            "-metadata:s:a:1",
            "title=application",
        ];
        args.extend_from_slice(COLOUR);
        run(&args, &source);
        let bytes = std::fs::read(&source).unwrap();
        let (mut edit, _) = tools.open(&source).unwrap();
        edit.trim(0, 1_600_000_000, 3_200_000_000).unwrap();
        let output = root.join(format!("{codec}-trim.mkv"));
        let report = export(&tools, &edit, &output, ExportProfile::default());
        assert_eq!(
            report.method,
            if codec == "pcm_s16le" {
                ExportMethod::StreamCopy
            } else {
                ExportMethod::SmartRender
            },
            "{report:?}"
        );
        assert_eq!(report.reencoded_video_frames, 0);
        let info = tools.probe(&output).unwrap();
        assert_eq!(info.audio.len(), 2);
        for track in 0..2 {
            let before = pcm(&tools, &source, track);
            let after = pcm(&tools, &output, track);
            let expected = &before[76_800 * 2..153_600 * 2];
            assert_eq!(
                after.len(),
                expected.len(),
                "{codec} track {track} sample count"
            );
            // Opus decodes to float; FLAC integer quantization can differ by one
            // 16-bit unit. This bound still detects a one-sample cut/phase error.
            let max_error = after
                .chunks_exact(2)
                .zip(expected.chunks_exact(2))
                .map(|(a, b)| {
                    (i16::from_le_bytes([a[0], a[1]]) as i32
                        - i16::from_le_bytes([b[0], b[1]]) as i32)
                        .abs()
                })
                .max()
                .unwrap_or(0);
            assert!(
                max_error <= if codec == "libopus" { 1 } else { 0 },
                "{codec} track {track} max sample error {max_error}"
            );
            if codec == "pcm_s16le" {
                assert_eq!(
                    hashes(&tools, &output, &format!("a:{track}")),
                    hashes(&tools, &source, &format!("a:{track}"))[75..150]
                );
            }
        }
        assert_eq!(std::fs::read(source).unwrap(), bytes);
    }
}

#[test]
#[ignore = "Requires local HEVC/AV1 FFmpeg encoders and a Studio evidence directory"]
fn hevc_main10_hdr_and_av1_packet_preservation() {
    let root = root("codecs");
    let tools = LocalTools::default();
    for (codec, profile) in [
        ("libx265", ExportProfile::Hevc { crf: 18 }),
        ("libaom-av1", ExportProfile::Av1 { crf: 24 }),
    ] {
        let source = root.join(format!("{codec}.mkv"));
        let mut args = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=10:duration=2",
            "-c:v",
            codec,
            "-vf",
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=limited",
            "-threads:v",
            "2",
            "-pix_fmt",
            "yuv420p10le",
            "-g",
            "10",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
            "-color_range",
            "tv",
        ];
        if codec == "libx265" {
            args.extend_from_slice(&["-x265-params", "pools=none:frame-threads=2:bframes=0:open-gop=0:scenecut=0:min-keyint=10:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400"]);
        } else {
            args.extend_from_slice(&["-cpu-used", "8", "-lag-in-frames", "0"]);
        }
        run(&args, &source);
        let bytes = std::fs::read(&source).unwrap();
        let (mut edit, _) = tools.open(&source).unwrap();
        edit.trim(0, 1_000_000_000, 2_000_000_000).unwrap();
        let output = root.join(format!("{codec}-trim.mkv"));
        let report = export(&tools, &edit, &output, profile);
        assert_eq!(report.method, ExportMethod::StreamCopy, "{report:?}");
        assert_eq!(
            hashes(&tools, &output, "v:0"),
            hashes(&tools, &source, "v:0")[10..20]
        );
        let info = tools.probe(&output).unwrap();
        assert_eq!(info.default_export_profile(), profile);
        assert_eq!(info.bit_depth, 10);
        assert_eq!(info.colour_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(
            info.hdr_metadata,
            tools.probe(&source).unwrap().hdr_metadata
        );
        assert_eq!(std::fs::read(source).unwrap(), bytes);
    }
}
