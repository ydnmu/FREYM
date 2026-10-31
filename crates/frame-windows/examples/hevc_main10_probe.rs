//! Read-only NVENC HEVC Main10 qualification. No capture or input frames are opened.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "qualified_h264_adapters={}",
        frame_windows::probe_native_encoders()?.len()
    );
    let candidates = frame_windows::probe_native_hevc_main10()?;
    for candidate in &candidates {
        println!(
            "{} luid={:016X} codec_config_bytes={}",
            candidate.adapter.description,
            candidate.adapter.luid,
            candidate.codec_config.len()
        );
    }
    println!("qualified_main10_adapters={}", candidates.len());
    Ok(())
}
