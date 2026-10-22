//! Unpaced synthetic MF transfer diagnostic, with opt-in bounded stage counters.
//! FREYM_MF_TRANSFER_FORMAT=nv12 selects the opt-in packed NV12 production path.
//! FREYM_MF_TRANSFER_TRANSPORT=shared_gpu opts into same-adapter NT handle sharing.
//! FREYM_MF_PROBE_PATTERN=1 selects four changing color/edge pattern phases.
//! FREYM_MF_TRANSFER_METRICS=1 enables counters. No screen/audio/camera source opens.

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use frame_clock::MediaTimestamp;
    use frame_mux::{ContainerWriter, MkvVideoWriter};
    use frame_windows::{MfStageTiming, NativeEncoderConfig};
    use std::{num::NonZeroU32, path::PathBuf, time::Instant};

    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: mf_transfer_probe OUTPUT_DIRECTORY WIDTH HEIGHT FRAMES (1–240)".into());
    }
    let output = PathBuf::from(&args[0]);
    let width: u32 = args[1].to_string_lossy().parse()?;
    let height: u32 = args[2].to_string_lossy().parse()?;
    let frames: u32 = args[3].to_string_lossy().parse()?;
    std::fs::create_dir_all(&output)?;
    let adapter = frame_windows::enumerate_native_adapters()?
        .into_iter()
        .find(|a| matches!(a.vendor_id, 0x8086 | 0x1002))
        .ok_or("No Intel or AMD adapter is present.")?;
    let config = NativeEncoderConfig::proof(
        width,
        height,
        NonZeroU32::new(60).unwrap(),
        NonZeroU32::new(1).unwrap(),
    )?;
    let path = output.join("synthetic.mkv");
    let mut writer = None;
    let started = Instant::now();
    let (session, transfer) = frame_windows::synthetic_media_foundation_h264_sequence(
        &adapter,
        &config,
        frames,
        |info, packet| {
            if writer.is_none() {
                writer = Some(MkvVideoWriter::create(&path, info, MediaTimestamp(0))?);
            }
            writer.as_mut().expect("writer created").write(packet)
        },
    )?;
    let sequence_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let artifact = writer
        .ok_or("The synthetic sequence produced no file.")?
        .finalize()?;
    let stage = |timing: MfStageTiming| {
        serde_json::json!({
            "samples": timing.samples,
            "mean_us": (timing.samples != 0).then(|| timing.total_ns as f64 / timing.samples as f64 / 1_000.0),
            "p50_upper_us": timing.percentile_upper_us(50),
            "p95_upper_us": timing.percentile_upper_us(95),
            "p99_upper_us": timing.percentile_upper_us(99),
            "max_us": timing.max_ns as f64 / 1_000.0,
        })
    };
    let stages = transfer.map(|t| {
        serde_json::json!({
            "conversion": stage(t.conversion),
            "readback": stage(t.readback), "ipc_roundtrip": stage(t.ipc_roundtrip),
            "helper_receive": stage(t.helper_receive), "helper_upload": stage(t.helper_upload),
            "shared_gpu_copy": stage(t.shared_gpu_copy),
            "helper_shared_gpu_copy": stage(t.helper_shared_gpu_copy),
        })
    });
    let report = serde_json::json!({
        "workload": if std::env::var_os("FREYM_MF_PROBE_PATTERN").is_some_and(|value| value == "1") {
            "four generated color/checkerboard/thin-line phases, copied into leased GPU slots, unpaced; no capture/audio"
        } else { "four immutable solid-color GPU surfaces, unpaced; no capture/audio" },
        "adapter": adapter.description, "adapter_luid": adapter.luid,
        "width": width, "height": height, "requested_frames": frames,
        "nominal_rate": "60/1", "sequence_ms_including_prepare_and_stop": sequence_ms,
        "accepted_frames": session.accepted_inputs, "completed_frames": session.completed_frames,
        "native_failures": session.native_failures,
        "discarded_on_stop": session.discarded_on_stop,
        "discarded_on_failure": session.discarded_on_failure,
        "discarded_on_unwind": session.discarded_on_unwind,
        "transfer": transfer, "stages": stages, "artifact": artifact.path,
        "percentiles": "fixed logarithmic bucket upper bounds, not exact quantiles",
        "timing_scope": "CPU-observed overlapping stages; upload does not measure GPU completion",
        "shared_gpu_scope": "BGRA shared NT texture -> helper-private BGRA -> helper NV12 DXGI sample; GPU copies/conversion remain. Registration failure uses the selected CPU format. Mutex/protocol failure stops the stream.",
    });
    let file = std::fs::File::create_new(output.join("metrics.json"))?;
    serde_json::to_writer_pretty(file, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {}
