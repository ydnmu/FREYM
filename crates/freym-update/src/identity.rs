//! Stable application identity and settings directory.
//!
//! New installations use `org.freym.recorder`. If only the legacy
//! `dev.frame.recorder` settings directory exists, it is reused in place so
//! existing settings, shortcuts and native preferences remain available. No
//! copy or migration is performed.

use std::path::{Path, PathBuf};

pub const APP_ID: &str = "org.freym.recorder";
const LEGACY_APP_ID: &str = "dev.frame.recorder";

pub fn config_dir() -> Option<PathBuf> {
    dirs_next::config_dir().map(|root| resolve_config_dir(&root))
}

fn resolve_config_dir(root: &Path) -> PathBuf {
    let current = root.join(APP_ID);
    if current.is_dir() {
        return current;
    }
    let legacy = root.join(LEGACY_APP_ID);
    if legacy.is_dir() {
        return legacy;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_identity_wins_when_both_directories_exist() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(APP_ID)).unwrap();
        std::fs::create_dir(root.path().join(LEGACY_APP_ID)).unwrap();
        assert_eq!(resolve_config_dir(root.path()), root.path().join(APP_ID));
    }

    #[test]
    fn legacy_directory_is_reused_when_current_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(LEGACY_APP_ID);
        std::fs::create_dir(&legacy).unwrap();
        for name in [
            "recording-settings.json",
            "shortcuts.json",
            "native-ui.json",
        ] {
            std::fs::write(legacy.join(name), name).unwrap();
        }
        let resolved = resolve_config_dir(root.path());
        assert_eq!(resolved, legacy);
        for name in [
            "recording-settings.json",
            "shortcuts.json",
            "native-ui.json",
        ] {
            assert_eq!(std::fs::read_to_string(resolved.join(name)).unwrap(), name);
        }
        assert!(!root.path().join(APP_ID).exists());
    }

    #[test]
    fn new_install_uses_the_current_identity() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(resolve_config_dir(root.path()), root.path().join(APP_ID));
    }
}
