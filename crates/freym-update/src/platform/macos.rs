use anyhow::{Result, bail};
use std::path::Path;
pub fn app_name() -> &'static str {
    "freym"
}
pub fn package() -> &'static str {
    "macos-app"
}
pub fn target() -> String {
    format!("macos-{}", std::env::consts::ARCH)
}
pub fn validate(_: &Path) -> Result<()> {
    bail!(
        "Signed and notarized macOS bundle updates are not configured; use the application distributor"
    )
}
