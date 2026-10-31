// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Bounded real Desktop Duplication comparison over the same animated GDI source.
//! Usage: live_mf_comparison ABSOLUTE_OUTPUT_DIR cpu|shared_gpu off|on SECONDS [FPS] [mf|native]
//! Source timings measure CPU animation/paint submission, not GPU/game frame time.
#![cfg_attr(target_os = "windows", allow(unsafe_code, dead_code))]
#![recursion_limit = "256"] // Bounded diagnostic JSON schema expansion.

#[cfg(target_os = "windows")]
include!("support/pattern.rs");

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Windows live capture required");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
fn distribution(mut samples: Vec<u64>) -> serde_json::Value {
    samples.sort_unstable();
    if samples.is_empty() {
        return serde_json::Value::Null;
    }
    let at = |p: usize| samples[(samples.len() * p).div_ceil(100).saturating_sub(1)] as f64 / 1e6;
    serde_json::json!({"samples": samples.len(), "p50_ms": at(50), "p95_ms": at(95), "p99_ms": at(99), "max_ms": at(100)})
}

#[cfg(target_os = "windows")]
fn preview_timing(timing: frame_windows::gpu_preview::PreviewStageTiming) -> serde_json::Value {
    serde_json::json!({"histogram": timing, "p95_upper_us": timing.percentile_upper_us(95),
        "p99_upper_us": timing.percentile_upper_us(99)})
}

#[cfg(target_os = "windows")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use frame_capture::{
        CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame, DisplayBackend,
    };
    use frame_clock::MasterClock;
    use frame_contracts::{CaptureTarget, FpsMode};
    use frame_encode::{
        EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, duration_until, nominal_duration,
    };
    use frame_mux::{ContainerWriter, MkvVideoWriter};
    use frame_windows::gpu_preview::{GpuPreviewPresenter, GpuPreviewSink};
    use frame_windows::preview::{PreviewConfig, new_hub};
    use frame_windows::{
        NativeEncoderConfig, PlatformBackend, WindowsCaptureSession, WindowsH264Encoder,
        WindowsSurface,
    };
    use std::{
        num::NonZeroU32,
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(4..=6).contains(&args.len())
        || !["cpu", "shared_gpu"].contains(&args[1].as_str())
        || !["off", "on"].contains(&args[2].as_str())
    {
        return Err(
            "usage: live_mf_comparison ABSOLUTE_OUTPUT_DIR cpu|shared_gpu off|on SECONDS [FPS] [mf|native]"
                .into(),
        );
    }
    let output = PathBuf::from(&args[0]);
    let seconds: u64 = args[3].parse()?;
    let fps: u32 = args.get(4).map_or(Ok(60), |value| value.parse())?;
    let backend = args.get(5).map_or("mf", String::as_str);
    if !["mf", "native"].contains(&backend) || backend == "native" && args[1] != "cpu" {
        return Err("Backend must be mf or native; native preview mode has no shared-GPU transport comparison".into());
    }
    if !output.is_absolute()
        || !(2..=30).contains(&seconds)
        || !(1..=480).contains(&fps)
        || output.exists()
    {
        return Err(
            "Output must be new/absolute; duration 2..30 seconds; requested FPS 1..480".into(),
        );
    }
    // Before any native thread/helper is started; never mutate process routing in a live session.
    unsafe {
        std::env::set_var("FREYM_MF_TRANSFER_TRANSPORT", &args[1]);
        std::env::set_var("FREYM_MF_TRANSFER_FORMAT", "bgra8");
        std::env::set_var("FREYM_MF_TRANSFER_METRICS", "1");
    }
    std::fs::create_dir(&output)?;
    let displays = PlatformBackend.enumerate_displays()?;
    let supported: Vec<_> = displays
        .iter()
        .filter_map(|display| {
            frame_windows::display_capture_adapter_identity(&display.id)
                .ok()
                .filter(|adapter| {
                    backend == "native" || matches!(adapter.vendor_id, 0x8086 | 0x1002)
                })
                .map(|adapter| (display, adapter))
        })
        .collect();
    let (display, adapter) = supported
        .iter()
        .find(|(display, _)| display.is_primary)
        .or_else(|| supported.first())
        .ok_or_else(|| {
            format!(
                "No attached display on an Intel/AMD MF adapter. Actual display adapters: {:?}",
                displays
                    .iter()
                    .map(|display| (
                        &display.id.0,
                        frame_windows::display_capture_adapter_identity(&display.id)
                    ))
                    .collect::<Vec<_>>()
            )
        })?;
    let preview = args[2] == "on";
    let fixture = pattern::PatternWindow::show_with_preview(
        pattern::PatternSpec {
            stripe_width: 80,
            palette: [
                [255, 0, 0],
                [0, 255, 0],
                [0, 0, 255],
                [255, 255, 0],
                [0, 255, 255],
                [255, 0, 255],
            ],
            band_color: [16, 16, 16],
            band_height: display.height * 3 / 5,
        },
        display.x,
        display.y,
        display.width,
        display.height,
        preview,
    )?;
    let sink = GpuPreviewSink::new(1);
    let mut presenter = if preview {
        Some(GpuPreviewPresenter::start(&sink, fixture.child)?)
    } else {
        None
    };
    let hub = new_hub(PreviewConfig::default(), 1)?;
    if preview {
        hub.set_gpu_sink(Some(Arc::clone(&sink)))?;
    }
    std::thread::sleep(Duration::from_millis(500));
    fixture.check_alive()?;
    let clock = Arc::new(MasterClock::default());
    let target = CaptureTarget::Display(display.id.clone());
    let context = CaptureContext {
        clock: Arc::clone(&clock),
        cancellation: CaptureCancellation::default(),
    };
    let mut capture = if preview {
        WindowsCaptureSession::start_with_preview_hub_cursor(&target, context, hub, false)?
    } else {
        frame_windows::start_display_with_cursor(&target, FpsMode::AutoMatchSource, context, false)?
    };
    let first_deadline = Instant::now() + Duration::from_secs(5);
    let first = loop {
        if let Some(frame) = capture.try_frame()? {
            break frame;
        }
        if Instant::now() >= first_deadline {
            return Err("No completed capture surface in 5 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let config = NativeEncoderConfig::proof(
        display.width,
        display.height,
        NonZeroU32::new(fps).unwrap(),
        NonZeroU32::new(1).unwrap(),
    )?;
    let mut encoder = WindowsH264Encoder::open(config, &first.surface)?;
    let encoder_id = encoder.info().capability.id.clone();
    let mut writer =
        MkvVideoWriter::create(&output.join("capture.mkv"), encoder.info(), first.timestamp)?;
    let recording_origin_ns = first.timestamp.0;
    let started = Instant::now();
    let warmup_end = started + Duration::from_secs(1);
    let deadline = warmup_end + Duration::from_secs(seconds);
    let mut measurement_origin = None;
    let mut capture_before = capture.stats();
    let mut encoder_before = encoder.stats();
    const MAX_SAMPLES: usize = 32_768;
    let mut intervals = Vec::with_capacity(MAX_SAMPLES);
    let mut latency = Vec::with_capacity(MAX_SAMPLES);
    let mut output_pts_intervals = Vec::with_capacity(MAX_SAMPLES);
    let mut output_delivery_intervals = Vec::with_capacity(MAX_SAMPLES);
    let mut interval_overflow = 0u64;
    let mut latency_overflow = 0u64;
    let mut output_pts_overflow = 0u64;
    let mut output_delivery_overflow = 0u64;
    let mut previous_pts = None;
    let mut previous_output_pts = None;
    let mut previous_output_delivery = None;
    let mut steady_pts_packets = 0u64;
    let mut lookahead = Some(first);
    let mut pending: Option<EncoderInput<CapturedFrame<WindowsSurface>>> = None;
    let mut eos = false;
    let mut drain_started = None;
    let mut last_progress = Instant::now();
    let mut packets = 0u64;
    let mut marker = 0;
    let mut steady_end = None;
    let mut encoder_steady_end = None;
    let mut preview_before = sink.stats();
    let mut preview_steady_end = None;
    let mut measurement_end = None;
    let mut source_samples = Vec::new();
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        loop {
            fixture.check_alive()?;
            if let Some(error) = sink.error() {
                return Err(format!("Preview failed during measurement: {error}").into());
            }
            let now = Instant::now();
            if measurement_origin.is_none() && now >= warmup_end {
                measurement_origin = Some(clock.now());
                capture_before = capture.stats();
                encoder_before = encoder.stats();
                preview_before = sink.stats();
                sink.set_timing_enabled(true);
                fixture.reset_timing();
            }
            let stopping = now >= deadline;
            if stopping && steady_end.is_none() {
                steady_end = Some(capture.stats());
                encoder_steady_end = Some(encoder.stats());
                measurement_end = Some(clock.now());
                sink.set_timing_enabled(false);
                preview_steady_end = Some(sink.stats());
                source_samples = fixture.timing();
            }
            for _ in 0..frame_windows::MAX_PACKETS {
                match encoder.try_packet()? {
                    EncoderPoll::Packet(packet) => {
                        if measurement_origin.is_some_and(|origin| packet.pts >= origin)
                            && measurement_end.is_none_or(|end| packet.pts < end)
                        {
                            let delivered_ns = clock.now().0;
                            let sample = delivered_ns
                                .checked_sub(packet.pts.0)
                                .ok_or("Packet timestamp is in the future")?;
                            if latency.len() < MAX_SAMPLES {
                                latency.push(sample);
                            } else {
                                latency_overflow += 1;
                            }
                            steady_pts_packets += 1;
                            if let Some(previous) = previous_output_pts {
                                let interval = packet
                                    .pts
                                    .0
                                    .checked_sub(previous)
                                    .filter(|interval| *interval > 0)
                                    .ok_or("Encoder output PTS repeated or regressed")?;
                                if output_pts_intervals.len() < MAX_SAMPLES {
                                    output_pts_intervals.push(interval);
                                } else {
                                    output_pts_overflow += 1;
                                }
                            }
                            if let Some(previous) = previous_output_delivery {
                                let interval = delivered_ns
                                    .checked_sub(previous)
                                    .ok_or("Packet delivery clock regressed")?;
                                if output_delivery_intervals.len() < MAX_SAMPLES {
                                    output_delivery_intervals.push(interval);
                                } else {
                                    output_delivery_overflow += 1;
                                }
                            }
                            previous_output_pts = Some(packet.pts.0);
                            previous_output_delivery = Some(delivered_ns);
                        }
                        writer.write(packet)?;
                        packets += 1;
                    }
                    EncoderPoll::Pending => break,
                    EncoderPoll::Drained => return Ok(()),
                }
            }
            if let Some(input) = pending.take()
                && let SubmitOutcome::Backpressure(input) = encoder.submit(input)?
            {
                pending = Some(input);
            }
            if pending.is_none() && !eos {
                if stopping {
                    if let Some(last) = lookahead.take() {
                        pending = Some(EncoderInput {
                            input: last,
                            duration_ns: nominal_duration(
                                encoder.config().rate_num,
                                encoder.config().rate_den,
                            ),
                        });
                    } else {
                        encoder.begin_drain()?;
                        eos = true;
                        drain_started = Some(Instant::now());
                    }
                } else if let Some(next) = capture.try_frame()? {
                    if measurement_origin.is_some_and(|origin| next.timestamp >= origin) {
                        if let Some(previous) = previous_pts {
                            let sample = next
                                .timestamp
                                .0
                                .checked_sub(previous)
                                .filter(|interval| *interval > 0)
                                .ok_or("Capture timestamp repeated or regressed")?;
                            if intervals.len() < MAX_SAMPLES {
                                intervals.push(sample);
                            } else {
                                interval_overflow += 1;
                            }
                        }
                        previous_pts = Some(next.timestamp.0);
                    }
                    if let Some(previous) = lookahead.replace(next) {
                        pending = Some(EncoderInput {
                            duration_ns: duration_until(
                                previous.timestamp,
                                lookahead.as_ref().unwrap().timestamp,
                            )?,
                            input: previous,
                        });
                    }
                }
            }
            let next_marker =
                capture.stats().counters.dequeued + encoder.stats().accepted_inputs + packets;
            if next_marker != marker {
                marker = next_marker;
                last_progress = Instant::now();
            }
            if (pending.is_some() || eos || encoder.stats().accepted_inputs > packets)
                && last_progress.elapsed() > Duration::from_secs(5)
            {
                return Err("Native progress watchdog expired".into());
            }
            if drain_started.is_some_and(|time| time.elapsed() > Duration::from_secs(10)) {
                return Err("Drain bound expired".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    // A failed encoder is released before capture, including error paths.
    let encoder_stop = encoder.stop();
    let capture_stop = capture.stop();
    if let Some(presenter) = presenter.as_mut() {
        presenter.close();
    }
    result?;
    encoder_stop?;
    capture_stop?;
    if let Some(error) = sink.error() {
        return Err(format!("Preview failed before shutdown: {error}").into());
    }
    let c = steady_end.ok_or("Measurement did not finish")?.counters;
    let b = capture_before.counters;
    let e = encoder.stats();
    let steady_e = encoder_steady_end.ok_or("Missing encoder steady snapshot")?;
    let p = sink.stats();
    let steady_p = preview_steady_end.ok_or("Missing preview steady snapshot")?;
    let source = source_samples;
    let artifact = writer.finalize()?;
    let transfer = encoder.mf_transfer_stats();
    if backend == "mf" && transfer.is_none() {
        return Err("Not the isolated MF path; comparison unsupported on this adapter".into());
    }
    let window_ns = measurement_end
        .unwrap()
        .0
        .checked_sub(measurement_origin.unwrap().0)
        .filter(|window| *window >= 1_000_000_000)
        .ok_or("Rate measurement requires a positive real window of at least one second")?;
    let measured_hz = |events: u64| events as f64 * 1e9 / window_ns as f64;
    let report = serde_json::json!({
        "schema_version": 3,
        "comparison_kind": if backend == "mf" { "mf_transport" } else { "native_preview" }, "encoder_id": encoder_id,
        "source": "same 8ms animated GDI pattern via real Desktop Duplication", "display_id": display.id.0,
        "source_marker_period_ns": 8_000_000,
        "source_marker": {"width": 64, "step": 8, "sample_row": display.height - 40},
        "width": display.width, "height": display.height, "nominal_encoder_hz": fps, "seconds": seconds, "warmup_seconds": 1,
        "adapter": {"luid": adapter.luid, "vendor_id": adapter.vendor_id, "description": adapter.description,
            "driver_version": frame_windows::adapter_driver_version(adapter.luid).ok()},
        "measurement_begin_ns": measurement_origin.map(|value| value.0), "measurement_end_ns": measurement_end.map(|value| value.0),
        "recording_origin_ns": recording_origin_ns,
        "transport": args[1], "preview": preview, "preview_limit_hz": 30,
        "preview_host_excluded_from_capture": preview,
        "capture_interval": distribution(intervals), "capture_to_packet_latency": distribution(latency),
        "sample_overflow": {"interval": interval_overflow, "latency": latency_overflow,
            "output_pts_interval": output_pts_overflow, "output_delivery_interval": output_delivery_overflow},
        "frame_pacing": {"encoder_output_pts_interval": distribution(output_pts_intervals),
            "packet_delivery_interval": distribution(output_delivery_intervals), "steady_pts_packets": steady_pts_packets,
            "scope": "native capture/output PTS retain source pacing; delivery intervals include caller polling, bursts and drain; requested FPS is not a pacing clock"},
        "pipeline_rates": {"window_ns": window_ns,
            "source_native_presentations_hz": measured_hz(c.native_desktop_presentations - b.native_desktop_presentations),
            "capture_dequeued_hz": measured_hz(c.dequeued - b.dequeued),
            "encoder_accepted_hz": measured_hz(steady_e.accepted_inputs - encoder_before.accepted_inputs),
            "encoder_completed_hz": measured_hz(steady_e.completed_frames - encoder_before.completed_frames),
            "source_cpu_paint_submissions_hz": measured_hz(source.len() as u64),
            "nominal_display_hz": capture_before.nominal_source_hz,
            "requested_encoder_hz": fps,
            "scope": "separate observed counter deltas / actual steady window; source native timestamp identity is not pixel-content uniqueness; not encoder capacity under an unsaturated source"},
        "high_fps_acceptance": false,
        "source_pixel_content_uniqueness": serde_json::Value::Null,
        "latency_scope": "steady-window packet PTS; delivery may occur during drain; includes lookahead and caller poll scheduling",
        "source_cpu_update_interval": distribution(source.iter().map(|sample| sample.0).collect()),
        "source_cpu_paint_submission": distribution(source.iter().map(|sample| sample.1).collect()),
        "source_gpu_completion_frame_time": serde_json::Value::Null,
        "capture_steady": {"updates": c.updates_with_image - b.updates_with_image, "dequeued": c.dequeued - b.dequeued,
            "native_desktop_presentations": c.native_desktop_presentations - b.native_desktop_presentations,
            "pool_exhausted": c.pool_exhausted - b.pool_exhausted, "ready_evictions": c.ready_evictions - b.ready_evictions,
            "worker_contention_drops": c.worker_contention_drops - b.worker_contention_drops, "upstream_accumulated_updates": c.upstream_accumulated_updates - b.upstream_accumulated_updates},
        "encoder_steady": {"accepted": steady_e.accepted_inputs - encoder_before.accepted_inputs, "completed": steady_e.completed_frames - encoder_before.completed_frames},
        "encoder_session": {"accepted": e.accepted_inputs, "completed": e.completed_frames, "packets_written": packets},
        "preview_session": {"copied": p.copied, "presented": p.presented, "rate_limited": p.rate_limited, "producer_busy": p.producer_busy, "presenter_busy": p.presenter_busy, "copy_submit_cpu_ns": p.copy_submit_nanos},
        "preview_steady": {"copied": steady_p.copied - preview_before.copied, "presented": steady_p.presented - preview_before.presented,
            "rate_limited": steady_p.rate_limited - preview_before.rate_limited, "producer_busy": steady_p.producer_busy - preview_before.producer_busy,
            "presenter_busy": steady_p.presenter_busy - preview_before.presenter_busy,
            "replaced_pending": steady_p.replaced_pending - preview_before.replaced_pending},
        "preview_timing_scope": "CPU call/notification age, fixed histograms enabled only during steady window; not GPU completion or display scanout; live snapshots non-atomic",
        "preview_timing": {"copy_submit": preview_timing(p.copy_submit_cpu), "mailbox_notification_age": preview_timing(p.mailbox_wait_cpu), "presenter_submit": preview_timing(p.presenter_submit_cpu)},
        "preview_gpu_command_span": preview_timing(p.presenter_gpu_span),
        "preview_gpu_query_scope": "four timestamp/disjoint sets around presenter CopyResource/Flush, ends before Present; DONOTFLUSH reads, no spin; includes submission/resource waits, not source application or display scanout",
        "preview_gpu_query_missing": {"unsupported": p.gpu_timing_unavailable, "invalid": p.gpu_timing_invalid, "busy": p.gpu_timing_busy, "unresolved_at_close": p.gpu_timing_unresolved},
        "transfer_scope": "whole encoder session including warmup and drain; CPU-observed timings, not GPU completion",
        "transfer": transfer, "transfer_p95_upper_us": {"readback": transfer.and_then(|value| value.readback.percentile_upper_us(95)), "ipc": transfer.and_then(|value| value.ipc_roundtrip.percentile_upper_us(95)), "gpu_copy": transfer.and_then(|value| value.shared_gpu_copy.percentile_upper_us(95))},
        "transfer_p99_upper_us": {"readback": transfer.and_then(|value| value.readback.percentile_upper_us(99)), "ipc": transfer.and_then(|value| value.ipc_roundtrip.percentile_upper_us(99)), "gpu_copy": transfer.and_then(|value| value.shared_gpu_copy.percentile_upper_us(99))},
        "artifact": artifact,
    });
    std::fs::write(
        output.join("metrics.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
