# Architecture

FREYM Alpha is a native Rust/GPUI recorder. freym-app and freym-ui own the UI;
frame-runtime::RecordingControl coordinates capture, audio, encoding, preview,
MKV output, pause/resume/stop and recovery. Contracts, clock, settings, metrics
and metadata support the recording lifecycle. Platform crates own OS resources.
freym-platform provides desktop integration; freym-update supplies the existing
launcher and disabled-by-default verified update infrastructure.

This public snapshot starts with Alpha recording core. Features developed in
private may enter public main in Beta and later releases when ready.

Preserve the pinned GPUI Windows renderer patch, codec headers, fonts and legal
notices. Cargo.toml and Cargo.lock define the build graph.
