use anyhow::{Result, ensure};
use std::path::Path;
pub fn app_name() -> &'static str {
    "freym"
}
pub fn package() -> &'static str {
    "linux-portable-exe"
}
pub fn target() -> String {
    format!("linux-{}", std::env::consts::ARCH)
}
pub fn validate(path: &Path) -> Result<()> {
    ensure!(
        std::env::var_os("APPIMAGE").is_none()
            && std::env::var_os("FLATPAK_ID").is_none()
            && std::env::var_os("SNAP").is_none(),
        "AppImage, Flatpak and Snap require their own update mechanism"
    );
    ensure!(
        ![
            "/usr", "/bin", "/sbin", "/lib", "/opt", "/nix", "/app", "/snap"
        ]
        .iter()
        .any(|p| path.starts_with(p)),
        "System installations must use their package manager"
    );
    ensure!(
        !path
            .components()
            .any(|part| [".linuxbrew", "linuxbrew", ".cargo"]
                .iter()
                .any(|name| part.as_os_str() == *name)),
        "Package-manager-owned installations must use their package manager"
    );
    Ok(())
}
