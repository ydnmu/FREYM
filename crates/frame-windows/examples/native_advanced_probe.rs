//! Deterministic, capture-free native Advanced proof on the production packet encoder.
#[cfg(windows)]
use frame_contracts::*;
#[cfg(windows)]
use frame_encode::EncodedStreamInfo;
#[cfg(windows)]
use frame_mux::{ContainerWriter, MkvVideoWriter};
#[cfg(windows)]
use frame_windows::*;
#[cfg(windows)]
use std::{num::NonZeroU32, path::PathBuf};

#[cfg(not(windows))]
fn main() {
    eprintln!("This acceptance example requires Windows NVENC.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--frame-mf-helper") {
        std::process::exit(run_mf_helper());
    }
    let out = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("New absolute evidence directory required")?,
    );
    if !out.is_absolute() || out.exists() {
        return Err("Use a new absolute evidence directory".into());
    }
    std::fs::create_dir(&out)?;
    let temporal_proof = std::env::args().nth(2).as_deref() == Some("--temporal");
    let adapter = enumerate_native_adapters()?
        .into_iter()
        .find(|a| a.vendor_id == 0x10de)
        .ok_or("No NVIDIA adapter")?;
    std::fs::write(
        out.join("adapter.json"),
        serde_json::to_vec_pretty(&adapter)?,
    )?;
    let mut rows = Vec::new();
    for (codec, profile) in [
        ("h264", NativeVideoProfile::H264High),
        ("av1", NativeVideoProfile::Av1Main8),
    ] {
        let mut cases = vec![
            ("automatic", VideoEncoderOverrides::default(), 8_000_000),
            (
                "cbr4",
                VideoEncoderOverrides {
                    rate_control: Some(VideoRateControl::Cbr),
                    ..Default::default()
                },
                4_000_000,
            ),
            (
                "cbr8",
                VideoEncoderOverrides {
                    rate_control: Some(VideoRateControl::Cbr),
                    ..Default::default()
                },
                8_000_000,
            ),
            (
                "vbr8",
                VideoEncoderOverrides {
                    rate_control: Some(VideoRateControl::Vbr),
                    ..Default::default()
                },
                8_000_000,
            ),
            (
                "cqp16",
                VideoEncoderOverrides {
                    rate_control: Some(VideoRateControl::Cqp),
                    qp: Some(16),
                    ..Default::default()
                },
                0,
            ),
            (
                "cqp32",
                VideoEncoderOverrides {
                    rate_control: Some(VideoRateControl::Cqp),
                    qp: Some(32),
                    ..Default::default()
                },
                0,
            ),
        ];
        for (name, multipass) in [
            ("quarter", VideoMultipass::QuarterResolution),
            ("full", VideoMultipass::FullResolution),
        ] {
            cases.push((
                name,
                VideoEncoderOverrides {
                    rate_control: Some(VideoRateControl::Vbr),
                    multipass: Some(multipass),
                    ..Default::default()
                },
                8_000_000,
            ));
        }
        for (name, spatial, temporal) in [
            ("spatial", true, false),
            ("temporal", false, true),
            ("aq-both", true, true),
        ] {
            cases.push((
                name,
                VideoEncoderOverrides {
                    spatial_aq: Some(spatial),
                    temporal_aq: Some(temporal),
                    ..Default::default()
                },
                8_000_000,
            ));
        }
        for (name, preset) in [
            ("p1", VideoEncoderPreset::P1),
            ("p2", VideoEncoderPreset::P2),
            ("p3", VideoEncoderPreset::P3),
            ("p4", VideoEncoderPreset::P4),
            ("p5", VideoEncoderPreset::P5),
            ("p6", VideoEncoderPreset::P6),
            ("p7", VideoEncoderPreset::P7),
        ] {
            cases.push((
                name,
                VideoEncoderOverrides {
                    preset: Some(preset),
                    ..Default::default()
                },
                8_000_000,
            ));
        }
        for (name, tuning) in [
            ("ull", VideoEncoderTuning::UltraLowLatency),
            ("ll", VideoEncoderTuning::LowLatency),
            ("hq", VideoEncoderTuning::HighQuality),
        ] {
            cases.push((
                name,
                VideoEncoderOverrides {
                    tuning: Some(tuning),
                    gop_frames: Some(30),
                    ..Default::default()
                },
                8_000_000,
            ));
        }
        if temporal_proof {
            cases.clear();
            for (name, on, high) in [
                ("hold-off-ull", false, false),
                ("hold-on-ull", true, false),
                ("hold-off-hq", false, true),
                ("hold-on-hq", true, true),
            ] {
                cases.push((
                    name,
                    VideoEncoderOverrides {
                        temporal_aq: Some(on),
                        preset: Some(if high {
                            VideoEncoderPreset::P5
                        } else {
                            VideoEncoderPreset::P1
                        }),
                        tuning: Some(if high {
                            VideoEncoderTuning::HighQuality
                        } else {
                            VideoEncoderTuning::UltraLowLatency
                        }),
                        ..Default::default()
                    },
                    2_000_000,
                ));
            }
        }
        for (name, overrides, bitrate) in cases {
            let mut config = NativeEncoderConfig::proof(
                1280,
                720,
                NonZeroU32::new(60).unwrap(),
                NonZeroU32::new(1).unwrap(),
            )?;
            config.encoder_overrides = overrides;
            config.gop_frames = overrides.gop_frames.unwrap_or(60);
            config.bitrate_bps = bitrate;
            config.peak_bitrate_bps = bitrate;
            config.rate_control = match overrides.rate_control {
                Some(VideoRateControl::Cqp) => NativeRateControl::Cqp,
                Some(VideoRateControl::Vbr) => {
                    config.peak_bitrate_bps = bitrate * 2;
                    NativeRateControl::Vbr
                }
                _ => NativeRateControl::Cbr,
            };
            let path = out.join(format!("{codec}-{name}.mkv"));
            let result = (|| -> Result<_, RecorderError> {
                probe_native_profile_for_adapter(&adapter, &config, profile)?;
                let mut writer: Option<MkvVideoWriter> = None;
                let mut bytes = 0u64;
                let mut keys = Vec::new();
                let mut packets = 0u64;
                let sequence = synthetic_encoder_sequence_with_pattern_period(
                    &adapter,
                    &config,
                    Some(&profile.id(adapter.luid)),
                    240,
                    if temporal_proof { 60 } else { 1 },
                    || false,
                    |info: &EncodedStreamInfo, packet| {
                        if writer.is_none() {
                            writer = Some(MkvVideoWriter::create(
                                &path,
                                info,
                                frame_clock::MediaTimestamp(0),
                            )?);
                        }
                        if packet.keyframe {
                            keys.push(packets);
                        }
                        bytes += packet.data.len() as u64;
                        packets += 1;
                        writer.as_mut().unwrap().write(packet)
                    },
                )?;
                writer.unwrap().finalize()?;
                Ok(
                    serde_json::json!({"codec":codec,"case":name,"request":config,"accepted":sequence.settings,
                    "qp":sequence.qp,"stats":format!("{:?}",sequence.stats),"elapsed_ns":sequence.elapsed_ns,
                    "latency_p95_ns":sequence.latency_p95_ns,"overhead_p95_ns":sequence.overhead_p95_ns,
                    "bytes":bytes,"packets":packets,"keyframe_packet_indices":keys}),
                )
            })();
            let row = match result {
                Ok(row) => row,
                Err(e) => serde_json::json!({"codec":codec,"case":name,"error":e}),
            };
            println!(
                "{codec}/{name}: {}",
                if row.get("error").is_some() {
                    "FAILED"
                } else {
                    "encoded"
                }
            );
            rows.push(row);
            std::fs::write(out.join("results.json"), serde_json::to_vec_pretty(&rows)?)?;
        }
    }
    if rows.iter().any(|r| r.get("error").is_some()) {
        return Err("Some native cases failed; see results.json".into());
    }
    Ok(())
}
