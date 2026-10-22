//! FRAME-1303: one synthetic BGRA GPU texture shared with a child process.
//! No WGC session, display or user device is opened.

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--shared-texture-child") {
        std::process::exit(frame_windows::run_shared_texture_proof_child());
    }
    if std::env::args().nth(1).as_deref() == Some("--watchdog-only") {
        println!(
            "{}",
            frame_windows::synthetic_shared_texture_watchdog_proof()?
        );
        return Ok(());
    }
    use frame_mux::ContainerWriter;

    let (info, packet, evidence) = frame_windows::synthetic_shared_texture_h264_frame()?;
    let path =
        std::env::temp_dir().join(format!("frame-shared-texture-{}.mkv", std::process::id()));
    let mut writer =
        frame_mux::MkvVideoWriter::create(&path, &info, frame_clock::MediaTimestamp(0))?;
    writer.write(packet)?;
    let artifact = writer.finalize()?;
    println!("{evidence}");
    println!("mkv={}", artifact.path);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {}
