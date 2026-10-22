use anyhow::{Result, ensure};
use std::path::Path;
pub fn app_name() -> &'static str {
    "freym.exe"
}
pub fn package() -> &'static str {
    "windows-portable-exe"
}
pub fn target() -> String {
    format!("windows-{}", std::env::consts::ARCH)
}
pub fn validate(path: &Path) -> Result<()> {
    let text = path.to_string_lossy().to_lowercase();
    ensure!(
        !text.contains("\\windowsapps\\")
            && !text.contains("\\scoop\\")
            && !text.contains("\\chocolatey\\")
            && !text.contains("\\winget\\"),
        "Package-manager-owned installations must use their package manager"
    );
    for name in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "SystemRoot",
    ] {
        if let Some(root) = std::env::var_os(name) {
            let root = root.to_string_lossy().to_lowercase();
            ensure!(
                !text.starts_with(&format!("{root}\\")),
                "System installations must use their installer or package manager"
            );
        }
    }
    Ok(())
}
