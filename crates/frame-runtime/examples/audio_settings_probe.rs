//! Read-only native audio discovery evidence; never starts capture or recording.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let audio = frame_runtime::audio_discovery::discover().map_err(|error| error.message)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "audioLevels": audio.audio_levels, "perApplicationAudio": audio.per_app_audio,
            "sources": audio.sources, "captureStarted": false,
        }))?
    );
    Ok(())
}
