//! Packaging policy is deliberately independent of the release manifest.
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
#[path = "platform/windows.rs"]
mod native;
#[cfg(target_os = "linux")]
#[path = "platform/linux.rs"]
mod native;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
#[path = "platform/macos.rs"]
mod native;
pub use native::{app_name, package, target};

pub fn portable_root(trust: &super::Trust, executable: &Path) -> Result<PathBuf> {
    ensure!(
        trust.package == package(),
        "This installation is managed externally; use its installer or package manager"
    );
    native::validate(executable)?;
    ensure!(
        executable
            .file_name()
            .is_some_and(|n| n == app_name() || n == launcher_name()),
        "Unsupported application filename"
    );
    let root = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Application directory is unavailable"))?;
    // Reject links/junctions in every ancestor rather than following a download
    // or package marker to an unrelated installation/user directory.
    for ancestor in executable.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        ensure!(
            !std::fs::symlink_metadata(ancestor)?
                .file_type()
                .is_symlink(),
            "Linked update installation is unsupported"
        );
    }
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("freym-portable.json"))?)?;
    ensure!(
        marker["schemaVersion"] == 1 && marker["package"] == package(),
        "Portable installation marker is missing or invalid"
    );
    ensure!(
        root.join(launcher_name()).is_file(),
        "Portable update launcher is missing"
    );
    Ok(root.to_path_buf())
}
pub fn launcher_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "freym-launcher.exe"
    } else {
        "freym-launcher"
    }
}
pub fn prepare_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = path;
    Ok(())
}
