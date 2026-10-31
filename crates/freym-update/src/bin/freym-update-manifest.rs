//! Combine an externally signed payload with its detached Ed25519 signature.
//! This tool never generates keys, signs releases or publishes files.
use anyhow::{Context, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "Input exceeds size limit");
    Ok(bytes)
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 3,
        "Usage: freym-update-manifest PAYLOAD_JSON DETACHED_SIGNATURE OUTPUT_JSON"
    );
    let trust = freym_update::trust()?;
    trust.validate().context(
        "Configure the real production endpoint and public keys before preparing an update feed",
    )?;
    let payload = read(Path::new(&args[0]), 128 * 1024)?;
    let signature = read(Path::new(&args[1]), 64)?;
    ensure!(
        signature.len() == 64,
        "Detached Ed25519 signature must contain exactly 64 raw bytes"
    );
    let envelope = serde_json::to_vec_pretty(&serde_json::json!({
        "schemaVersion": 1, "payload": BASE64_STANDARD.encode(payload),
        "signature": BASE64_STANDARD.encode(signature),
    }))?;
    freym_update::VerifiedRelease::verify(
        &envelope,
        &trust,
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        ),
    )?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[2])?;
    output.write_all(&envelope)?;
    output.sync_all()?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Could not prepare update manifest: {error:#}");
        std::process::exit(1);
    }
}
