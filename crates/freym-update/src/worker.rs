use crate::{Trust, VERSION, VerifiedRelease, install, manifest, platform};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Disabled,
    NotConfigured,
    Waiting,
    Checking,
    Current,
    Downloading { received: u64, total: u64 },
    Ready,
    Manual(String),
    Failed(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub installed: String,
    pub available: Option<String>,
    pub state: State,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            installed: VERSION.into(),
            available: None,
            state: State::Waiting,
        }
    }
}
impl Snapshot {
    pub fn message(&self) -> String {
        match &self.state {
            State::Disabled => "Automatic updates are off.".into(),
            State::NotConfigured => "Production update source and signing keys are not configured.".into(),
            State::Waiting => "Waiting to check for updates.".into(),
            State::Checking => "Checking for updates…".into(),
            State::Current => "You have the latest version.".into(),
            State::Downloading { received, total } => format!("Downloading update: {}%", received.saturating_mul(100) / total.max(&1)),
            State::Ready => "Update verified. Close Freym and open it with the portable launcher to use the update.".into(),
            State::Manual(reason) => reason.clone(),
            State::Failed(reason) => format!("Update failed: {reason}. Retry is available."),
        }
    }
    pub fn busy(&self) -> bool {
        matches!(self.state, State::Checking | State::Downloading { .. })
    }
}

pub struct Updater {
    trust: Trust,
    root: Option<PathBuf>,
    unsupported: Option<String>,
    enabled: AtomicBool,
    generation: AtomicU64,
    operation: Mutex<()>,
    status: Mutex<Snapshot>,
    schedule: Mutex<(Instant, u32)>,
}
impl Updater {
    /// Called on a background executor, after native preferences are loaded.
    pub fn new(enabled: bool) -> Result<Self> {
        let trust = crate::trust()?;
        let configured = trust.validate().is_ok();
        let location = std::env::current_exe().context("Application location is unavailable")?;
        let portable = platform::portable_root(&trust, &location);
        let (root, unsupported) = match portable {
            Ok(root) => (Some(root), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut status = Snapshot::default();
        let state = if !enabled {
            State::Disabled
        } else if !configured {
            State::NotConfigured
        } else {
            State::Waiting
        };
        status.state = state;
        if enabled
            && configured
            && let Some(root) = &root
        {
            let layout = install::Layout::new(root)?;
            if let Some(error) = layout.last_error()? {
                status.state = State::Failed(error);
            }
            if layout.ready().exists()
                && let Ok(release) = VerifiedRelease::verify(
                    &install::read_bounded(&layout.ready(), manifest::MAX_MANIFEST)?,
                    &trust,
                    Some(manifest::now()?),
                )
            {
                status.available = Some(release.manifest.version);
            }
        }
        Ok(Self {
            trust,
            root,
            unsupported,
            enabled: AtomicBool::new(enabled),
            generation: AtomicU64::new(0),
            operation: Mutex::new(()),
            status: Mutex::new(status),
            schedule: Mutex::new((Instant::now(), 0)),
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn state(&self, state: State) {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).state = state;
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.state(if enabled {
            State::Waiting
        } else {
            State::Disabled
        });
        *self.schedule.lock().unwrap_or_else(|p| p.into_inner()) = (Instant::now(), 0);
    }
    pub fn due(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
            && !matches!(self.snapshot().state, State::NotConfigured)
            && Instant::now() >= self.schedule.lock().unwrap_or_else(|p| p.into_inner()).0
    }
    fn active(&self, generation: u64) -> Result<()> {
        ensure!(
            self.enabled.load(Ordering::Acquire)
                && self.generation.load(Ordering::Acquire) == generation,
            "Update cancelled"
        );
        Ok(())
    }
    pub fn check(&self) {
        self.run_check(false);
    }
    pub fn check_now(&self) {
        self.run_check(true);
    }
    fn run_check(&self, manual: bool) {
        let Ok(_operation) = self.operation.try_lock() else {
            return;
        };
        if !manual && !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        if self.trust.validate().is_err() {
            self.state(State::NotConfigured);
            return;
        }
        self.state(State::Checking);
        let result = self.check_inner(generation);
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let mut schedule = self.schedule.lock().unwrap_or_else(|p| p.into_inner());
        let delay = match result {
            Ok(()) => {
                schedule.1 = 0;
                6 * 3600
            }
            Err(error) => {
                self.state(State::Failed(format!("{error:#}")));
                schedule.1 = schedule.1.saturating_add(1);
                match schedule.1 {
                    1 => 60,
                    2 => 300,
                    3 => 1800,
                    _ => 6 * 3600,
                }
            }
        };
        schedule.0 = Instant::now() + Duration::from_secs(delay);
    }
    fn agent(timeout: u64) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout)))
            .max_redirects(0)
            .https_only(true)
            .build()
            .into()
    }
    fn check_inner(&self, generation: u64) -> Result<()> {
        let mut response = Self::agent(15)
            .get(
                self.trust
                    .manifest_url
                    .as_deref()
                    .context("No update source")?,
            )
            .call()?;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(manifest::MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            self.generation.load(Ordering::Acquire) == generation,
            "Update cancelled"
        );
        let release = VerifiedRelease::verify(&bytes, &self.trust, Some(manifest::now()?))?;
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .available = Some(release.manifest.version.clone());
        if !release.newer_than(VERSION)? {
            if let Some(root) = &self.root {
                install::Layout::new(root)?.clear_error()?;
            }
            self.state(State::Current);
            return Ok(());
        }
        if !self.enabled.load(Ordering::Acquire) {
            self.state(State::Disabled);
            return Ok(());
        }
        let Some(root) = &self.root else {
            self.state(State::Manual(self.unsupported.clone().unwrap_or_else(
                || "Use the application distributor to update".into(),
            )));
            return Ok(());
        };
        let layout = install::Layout::new(root)?;
        let _download = layout.download_lock()?;
        layout.cleanup_downloads()?;
        ensure!(
            !layout.journal().exists(),
            "A previous update is awaiting launch verification"
        );
        let artifact = release.artifact()?;
        ensure!(
            artifact.min_launcher_protocol <= crate::LAUNCHER_PROTOCOL,
            "This update requires a newer portable launcher; use the distributor"
        );
        if layout.rejected().exists() {
            ensure!(
                install::read_bounded(&layout.rejected(), 128)? != artifact.sha256.as_bytes(),
                "This update previously failed to start and was rolled back; waiting for a newer release"
            );
        }
        // Reuse a fully verified completed download, including after relaunch.
        if layout.payload().exists() && manifest::verify_file(&layout.payload(), artifact).is_ok() {
            self.active(generation)?;
            // Reissue readiness from the fresh signed manifest without
            // downloading the same executable again on every periodic check.
            install::atomic_write(&layout.ready(), &release.envelope)?;
            self.state(State::Ready);
            layout.clear_error()?;
            return Ok(());
        }
        // Unpublish old readiness before touching its payload. Partial downloads
        // are private temporary files and can never become install candidates.
        if layout.ready().exists() {
            std::fs::remove_file(layout.ready())?;
        }
        let mut response = Self::agent(300).get(&artifact.url).call()?;
        let mut reader = response.body_mut().as_reader();
        let mut file = tempfile::Builder::new()
            .prefix(".freym-download-")
            .tempfile_in(&layout.directory)?;
        let mut received = 0_u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            self.active(generation)?;
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            received += n as u64;
            ensure!(
                received <= artifact.size,
                "Update download exceeds its signed size"
            );
            file.write_all(&buffer[..n])?;
            self.state(State::Downloading {
                received,
                total: artifact.size,
            });
        }
        file.as_file().sync_all()?;
        manifest::verify_file(file.path(), artifact)?;
        platform::prepare_executable(file.path())?;
        self.active(generation)?;
        file.persist(layout.payload())?;
        install::sync_directory(&layout.directory)?;
        // The launcher rechecks the persisted preference as well as every
        // signature/hash before it can replace an executable.
        self.active(generation)?;
        install::atomic_write(&layout.ready(), &release.envelope)?;
        layout.clear_error()?;
        self.state(State::Ready);
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct UpdatePreference {
    #[serde(default = "default_enabled")]
    pub automatic_updates: bool,
}
fn default_enabled() -> bool {
    true
}

pub fn preference_enabled() -> Result<bool> {
    preference_enabled_in(crate::identity::config_dir())
}

fn preference_enabled_in(config: Option<PathBuf>) -> Result<bool> {
    let path = config
        .context("User settings directory is unavailable")?
        .join("native-ui.json");
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<UpdatePreference>(&bytes)?.automatic_updates),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, prelude::BASE64_STANDARD};
    #[test]
    fn unreachable_https_endpoint_has_failure_backoff_and_disable_cancels_checks() {
        // Reserve and release a local port: no external release service is used.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let origin = format!("https://127.0.0.1:{port}");
        let updater = Updater {
            trust: Trust {
                schema_version: 1,
                manifest_url: Some(format!("{origin}/manifest.json")),
                public_keys: vec![
                    BASE64_STANDARD.encode(
                        ed25519_dalek::SigningKey::from_bytes(&[9; 32])
                            .verifying_key()
                            .as_bytes(),
                    ),
                ],
                allowed_origins: vec![origin],
                channel: "stable".into(),
                package: "disabled".into(),
            },
            root: None,
            unsupported: None,
            enabled: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            operation: Mutex::new(()),
            status: Mutex::new(Snapshot::default()),
            schedule: Mutex::new((Instant::now(), 0)),
        };
        let started = Instant::now();
        updater.check();
        assert!(started.elapsed() < Duration::from_secs(16));
        assert!(matches!(updater.snapshot().state, State::Failed(_)));
        assert!(!updater.due());
        updater.check(); // A manual retry bypasses the scheduled backoff.
        assert!(matches!(updater.snapshot().state, State::Failed(_)));
        assert_eq!(updater.schedule.lock().unwrap().1, 2);
        updater.set_enabled(false);
        assert!(updater.active(0).is_err());
        updater.check();
        assert_eq!(updater.snapshot().state, State::Disabled);
        assert!(!updater.due());
        updater.set_enabled(true);
        assert!(updater.due());
    }
    #[test]
    fn unavailable_settings_directory_keeps_launcher_updates_disabled() {
        let result = preference_enabled_in(None);
        assert!(result.is_err());
        assert!(!result.unwrap_or(false));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("native-ui.json"),
            br#"{"automatic_updates":false}"#,
        )
        .unwrap();
        assert!(!preference_enabled_in(Some(directory.path().to_owned())).unwrap());
    }

    #[test]
    fn old_native_preferences_default_to_enabled_and_explicit_false_survives() {
        let old: UpdatePreference =
            serde_json::from_str(r#"{"artwork":false,"countdown":5}"#).unwrap();
        assert!(old.automatic_updates);
        let off: UpdatePreference =
            serde_json::from_str(r#"{"automatic_updates":false,"artwork":true}"#).unwrap();
        assert!(!off.automatic_updates);
        let persisted = serde_json::to_vec(&off).unwrap();
        assert!(
            !serde_json::from_slice::<UpdatePreference>(&persisted)
                .unwrap()
                .automatic_updates
        );
        assert!(serde_json::from_str::<UpdatePreference>("{corrupt}").is_err());
    }
}
