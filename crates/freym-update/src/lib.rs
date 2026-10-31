//! Application updates only. Trust is compiled into the application, never supplied
//! by a feed, preferences, environment variable or downloaded executable.
pub mod install;
mod manifest;
pub mod platform;
mod worker;

pub use manifest::{Artifact, Manifest, Trust, VerifiedRelease};
pub use worker::{Snapshot, State, Updater};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LAUNCHER_PROTOCOL: u32 = 1;

pub fn trust() -> anyhow::Result<Trust> {
    Ok(serde_json::from_str(include_str!("../update-trust.json"))?)
}

pub fn show_launch_error(error: &anyhow::Error) {
    rfd::MessageDialog::new()
        .set_title("Freym could not open")
        .set_description(format!("{error:#}\n\nTry opening Freym again. If recovery still fails, reinstall it from the application distributor; keep your settings and recordings."))
        .set_level(rfd::MessageLevel::Error)
        .show();
}

#[cfg(test)]
mod tests;
