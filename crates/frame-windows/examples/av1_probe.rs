//! Bounded real AV1 qualification and three real display frames; no FPS/HDR claim.
#![cfg_attr(target_os = "windows", allow(unsafe_code, dead_code))]
#[cfg(target_os = "windows")]
include!("support/pattern.rs");
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Windows native AV1 probe required");
    std::process::exit(1);
}
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession, DisplayBackend};
    use frame_clock::MasterClock;
    use frame_contracts::{CaptureTarget, FpsMode};
    use frame_encode::{
        EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, duration_until, nominal_duration,
    };
    use frame_mux::{ContainerWriter, MkvVideoWriter};
    use frame_windows::{NativeEncoderConfig, PlatformBackend, WindowsH264Encoder};
    use std::{
        num::NonZeroU32,
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    let output = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: av1_probe NEW_ABSOLUTE_DIRECTORY")?,
    );
    if !output.is_absolute() || output.exists() {
        return Err("Output must be a new absolute directory".into());
    }
    std::fs::create_dir(&output)?;
    let (sdr, sdr_diagnostics) = frame_windows::probe_native_av1_diagnostics(false)?;
    let (hdr, hdr_diagnostics) = frame_windows::probe_native_av1_diagnostics(true)?;
    std::fs::write(
        output.join("qualification.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "sdr": sdr.iter().map(|candidate| serde_json::json!({"adapter": candidate.adapter, "codec_config_bytes": candidate.settings.codec_config.len()})).collect::<Vec<_>>(),
            "main10": hdr.iter().map(|candidate| serde_json::json!({"adapter": candidate.adapter, "codec_config_bytes": candidate.settings.codec_config.len()})).collect::<Vec<_>>(),
            "sdr_diagnostics": sdr_diagnostics, "main10_diagnostics": hdr_diagnostics,
            "hdr_capture_acceptance": false, "high_fps_acceptance": false
        }))?,
    )?;
    let display = PlatformBackend
        .enumerate_displays()?
        .into_iter()
        .find(|display| {
            frame_windows::display_capture_adapter_identity(&display.id).is_ok_and(|adapter| {
                sdr.iter()
                    .any(|candidate| candidate.adapter.luid == adapter.luid)
            })
        })
        .ok_or("No attached display on a qualified AV1 adapter")?;
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
        false,
    )?;
    std::thread::sleep(Duration::from_millis(300));
    fixture.check_alive()?;
    let mut capture = frame_windows::start_display_with_cursor(
        &CaptureTarget::Display(display.id.clone()),
        FpsMode::AutoMatchSource,
        CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: CaptureCancellation::default(),
        },
        false,
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = loop {
        if let Some(frame) = capture.try_frame()? {
            break frame;
        }
        if Instant::now() > deadline {
            return Err("No completed capture frame".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let config = NativeEncoderConfig::proof(
        display.width,
        display.height,
        NonZeroU32::new(60).unwrap(),
        NonZeroU32::new(1).unwrap(),
    )?;
    let mut encoder = WindowsH264Encoder::open_av1(config, &first.surface)?;
    std::fs::write(
        output.join("codec-config.obu"),
        &encoder.info().codec_config,
    )?;
    let mut writer =
        MkvVideoWriter::create(&output.join("capture.mkv"), encoder.info(), first.timestamp)?;
    let id = encoder.info().capability.id.clone();
    let mut frames = std::collections::VecDeque::from([first]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while frames.len() < 3 {
        if let Some(frame) = capture.try_frame()? {
            frames.push_back(frame);
        } else {
            if Instant::now() > deadline {
                return Err("Three completed capture frames were not available".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    while let Some(frame) = frames.pop_front() {
        let duration_ns = frames
            .front()
            .map(|next| duration_until(frame.timestamp, next.timestamp))
            .transpose()?
            .unwrap_or_else(|| nominal_duration(config.rate_num, config.rate_den));
        let mut input = EncoderInput {
            input: frame,
            duration_ns,
        };
        loop {
            match encoder.submit(input)? {
                SubmitOutcome::Accepted => break,
                SubmitOutcome::Backpressure(returned) => {
                    input = returned;
                    if Instant::now() > deadline {
                        return Err("Encoder admission timed out".into());
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    capture.stop()?;
    encoder.begin_drain()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut packets = 0;
    loop {
        match encoder.try_packet()? {
            EncoderPoll::Packet(packet) => {
                std::fs::write(output.join(format!("frame-{packets}.obu")), &packet.data)?;
                writer.write(packet)?;
                packets += 1;
            }
            EncoderPoll::Drained => break,
            EncoderPoll::Pending => {
                if Instant::now() > deadline {
                    return Err("AV1 drain timed out".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    encoder.stop()?;
    writer.finalize()?;
    if packets != 3 {
        return Err("Accepted source frames were not exactly accounted for".into());
    }
    let summary = frame_mux::recover(&output.join("capture.mkv"), &output.join("recovered.mkv"))?;
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"encoder": id, "captured_frames": 3, "accepted_frames": 3, "completed_packets": packets, "recovered_packets": summary.recovered_video_packets, "colour": "BT709 limited SDR", "width": display.width, "height": display.height, "physical_hdr_capture_acceptance": false, "throughput_acceptance": false}),
        )?,
    )?;
    println!(
        "three real AV1 capture frames encoded and recovered: {}",
        output.display()
    );
    Ok(())
}
