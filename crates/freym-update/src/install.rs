//! Portable updates replace one self-contained executable. The launcher and user
//! data are outside the replacement transaction. All mutation requires exclusive
//! application and download locks; running applications hold a shared lock.
use crate::{Trust, VerifiedRelease, manifest, platform};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub struct Layout {
    pub root: PathBuf,
    pub directory: PathBuf,
}
impl Layout {
    pub fn new(root: &Path) -> Result<Self> {
        let directory = root.join(".freym-update");
        fs::create_dir_all(&directory)?;
        let layout = Self {
            root: root.to_path_buf(),
            directory,
        };
        for path in [
            layout.directory.clone(),
            layout.app(),
            layout.payload(),
            layout.ready(),
            layout.journal(),
            layout.backup(),
            layout.healthy(),
            layout.receipt(),
            layout.rejected(),
            layout.directory.join("download.lock"),
            layout.directory.join("running.lock"),
            layout.directory.join("launcher.lock"),
            layout.directory.join("last-error.txt"),
        ] {
            match fs::symlink_metadata(path) {
                Ok(metadata) => ensure!(
                    !metadata.file_type().is_symlink(),
                    "Linked update files are unsupported"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(layout)
    }
    pub fn app(&self) -> PathBuf {
        self.root.join(platform::app_name())
    }
    pub fn payload(&self) -> PathBuf {
        self.directory.join("payload")
    }
    pub fn ready(&self) -> PathBuf {
        self.directory.join("ready.json")
    }
    pub fn journal(&self) -> PathBuf {
        self.directory.join("transaction.json")
    }
    pub fn backup(&self) -> PathBuf {
        self.directory.join("previous")
    }
    pub fn healthy(&self) -> PathBuf {
        self.directory.join("healthy")
    }
    pub fn receipt(&self) -> PathBuf {
        self.directory.join("installed.json")
    }
    pub fn rejected(&self) -> PathBuf {
        self.directory.join("rejected-sha256")
    }
    pub fn lock(&self, name: &str, shared: bool) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(name))?;
        if shared {
            file.try_lock_shared()?;
        } else {
            file.try_lock()?;
        }
        Ok(file)
    }
    pub fn download_lock(&self) -> Result<File> {
        self.lock("download.lock", false)
    }
    pub fn running_lock(&self) -> Result<File> {
        self.lock("running.lock", false)
    }
    pub fn record_error(&self, error: &anyhow::Error) {
        let _ = atomic_write(
            &self.directory.join("last-error.txt"),
            format!("{error:#}").as_bytes(),
        );
    }
    pub fn last_error(&self) -> Result<Option<String>> {
        let path = self.directory.join("last-error.txt");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(read_bounded(
            &path,
            manifest::MAX_MANIFEST,
        )?)?))
    }
    pub fn clear_error(&self) -> Result<()> {
        let path = self.directory.join("last-error.txt");
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
    pub fn cleanup_downloads(&self) -> Result<()> {
        cleanup_temps(&self.directory, &[".freym-download-"])
    }
    // Only with exclusive application/download locks, including after crashes.
    fn cleanup_interrupted(&self) -> Result<()> {
        cleanup_temps(&self.root, &[".freym-replace-"])?;
        cleanup_temps(
            &self.directory,
            &[".freym-download-", ".freym-replace-", ".freym-metadata-"],
        )
    }
}

fn cleanup_temps(directory: &Path, prefixes: &[&str]) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && prefixes
                .iter()
                .any(|p| entry.file_name().to_string_lossy().starts_with(p))
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    let _ = path;
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing update directory")?;
    let mut file = tempfile::Builder::new()
        .prefix(".freym-metadata-")
        .tempfile_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    sync_directory(parent)
}
fn copy_atomic(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("Missing application directory")?;
    let mut temp = tempfile::Builder::new()
        .prefix(".freym-replace-")
        .tempfile_in(parent)?;
    let mut source = File::open(source)?;
    std::io::copy(&mut source, &mut temp)?;
    temp.as_file()
        .set_permissions(source.metadata()?.permissions())?;
    temp.as_file().sync_all()?;
    temp.persist(destination)?;
    sync_directory(parent)
}
pub fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max,
        "Update metadata exceeds size limit"
    );
    Ok(bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    old_sha256: String,
    new_sha256: String,
    envelope: Vec<u8>,
    attempted: bool,
    nonce: String,
}
fn journal(layout: &Layout) -> Result<Option<Journal>> {
    if !layout.journal().exists() {
        return Ok(None);
    }
    let journal: Journal = serde_json::from_slice(&read_bounded(
        &layout.journal(),
        6 * manifest::MAX_MANIFEST,
    )?)?;
    ensure!(
        journal.schema_version == 1,
        "Unsupported installation journal"
    );
    Ok(Some(journal))
}
fn save_journal(layout: &Layout, journal: &Journal) -> Result<()> {
    atomic_write(&layout.journal(), &serde_json::to_vec(journal)?)
}
pub fn installed_version(layout: &Layout, trust: &Trust) -> Result<String> {
    if !layout.receipt().exists() {
        return Ok(crate::VERSION.into());
    }
    Ok(VerifiedRelease::verify(
        &read_bounded(&layout.receipt(), manifest::MAX_MANIFEST)?,
        trust,
        None,
    )?
    .manifest
    .version)
}

/// Caller holds exclusive running/download locks. Recovery ignores manifest
/// expiry: an interrupted transaction must remain recoverable while offline.
pub fn recover(layout: &Layout, trust: &Trust) -> Result<()> {
    let Some(journal) = journal(layout)? else {
        return Ok(());
    };
    let release = VerifiedRelease::verify(&journal.envelope, trust, None)?;
    ensure!(
        release.artifact()?.sha256 == journal.new_sha256,
        "Installation journal does not match the signed release"
    );
    if !layout.app().try_exists()? {
        ensure!(
            manifest::hash_file(&layout.backup())? == journal.old_sha256,
            "Rollback copy failed integrity verification"
        );
        copy_atomic(&layout.backup(), &layout.app())?;
        atomic_write(&layout.rejected(), journal.new_sha256.as_bytes())?;
        layout.record_error(&anyhow::anyhow!(
            "Installation was interrupted. The previous version was restored"
        ));
        if layout.ready().exists() {
            fs::remove_file(layout.ready())?;
        }
        fs::remove_file(layout.journal())?;
        return sync_directory(&layout.directory);
    }
    let current = manifest::hash_file(&layout.app())?;
    ensure!(
        current == journal.old_sha256 || current == journal.new_sha256,
        "Application was changed outside the update transaction; recovery requires the distributor"
    );
    if current == journal.old_sha256 {
        // Crash before replacement (or after rollback but before journal removal).
        fs::remove_file(layout.journal())?;
        return sync_directory(&layout.directory);
    }
    if !journal.attempted {
        return Ok(());
    }
    let healthy = layout.healthy().exists()
        && read_bounded(&layout.healthy(), 1024)? == journal.nonce.as_bytes();
    if healthy {
        atomic_write(&layout.receipt(), &journal.envelope)?;
        layout.clear_error()?;
    } else {
        ensure!(
            manifest::hash_file(&layout.backup())? == journal.old_sha256,
            "Rollback copy failed integrity verification"
        );
        copy_atomic(&layout.backup(), &layout.app())?;
        atomic_write(&layout.rejected(), journal.new_sha256.as_bytes())?;
        layout.record_error(&anyhow::anyhow!(
            "The updated application did not open successfully. The previous version was restored"
        ));
    }
    if layout.ready().exists() {
        fs::remove_file(layout.ready())?;
    }
    fs::remove_file(layout.journal())?;
    sync_directory(&layout.directory)
}

/// Caller holds exclusive running/download locks. No arbitrary paths or commands
/// from the feed are used. Readiness is published only after a full download.
pub fn install_ready(layout: &Layout, trust: &Trust) -> Result<bool> {
    ensure!(
        !layout.journal().exists(),
        "A previous installation still awaits verification"
    );
    if !layout.ready().exists() {
        return Ok(false);
    }
    let release = VerifiedRelease::verify(
        &read_bounded(&layout.ready(), manifest::MAX_MANIFEST)?,
        trust,
        Some(manifest::now()?),
    )?;
    ensure!(
        release.newer_than(&installed_version(layout, trust)?)?,
        "Update would not advance the installed version"
    );
    let artifact = release.artifact()?;
    ensure!(
        artifact.min_launcher_protocol <= crate::LAUNCHER_PROTOCOL,
        "Update requires a newer launcher"
    );
    if layout.rejected().exists() {
        ensure!(
            read_bounded(&layout.rejected(), 128)? != artifact.sha256.as_bytes(),
            "This update was rolled back; waiting for a newer release"
        );
    }
    manifest::verify_file(&layout.payload(), artifact)?;
    platform::prepare_executable(&layout.payload())?;
    copy_atomic(&layout.app(), &layout.backup())?;
    let transaction = Journal {
        schema_version: 1,
        old_sha256: manifest::hash_file(&layout.backup())?,
        new_sha256: artifact.sha256.clone(),
        envelope: release.envelope,
        attempted: false,
        nonce: format!("{}-{}", std::process::id(), manifest::now()?),
    };
    if layout.healthy().exists() {
        fs::remove_file(layout.healthy())?;
    }
    save_journal(layout, &transaction)?;
    copy_atomic(&layout.payload(), &layout.app())?;
    fs::remove_file(layout.ready())?;
    sync_directory(&layout.directory)?;
    Ok(true)
}

pub fn begin_launch(layout: &Layout) -> Result<Option<String>> {
    let Some(mut journal) = journal(layout)? else {
        return Ok(None);
    };
    journal.attempted = true;
    save_journal(layout, &journal)?;
    Ok(Some(journal.nonce))
}

/// A portable app instance holds this guard until all Recorder and Studio
/// shutdown work completes. Failure to acquire it never waits on the UI thread.
pub fn application_guard() -> Result<Option<File>> {
    let trust = crate::trust()?;
    let executable = std::env::current_exe()?;
    if trust.package == "disabled" {
        return Ok(None);
    }
    if executable
        .parent()
        .is_none_or(|root| !root.join("freym-portable.json").exists())
    {
        return Ok(None);
    }
    // A marked portable installation may be in a recovery transaction; failing
    // its validation must never open an unguarded second process.
    let root = platform::portable_root(&trust, &executable)?;
    Ok(Some(
        Layout::new(&root)?
            .lock("running.lock", true)
            .context("An update is being installed. Launch Freym again when it finishes")?,
    ))
}

/// Called on a background worker only after the native Recorder window opened.
pub fn mark_healthy() -> Result<()> {
    let Some(nonce) = std::env::var_os("FREYM_UPDATE_HEALTH") else {
        return Ok(());
    };
    let trust = crate::trust()?;
    let root = platform::portable_root(&trust, &std::env::current_exe()?)?;
    let layout = Layout::new(&root)?;
    let Some(journal) = journal(&layout)? else {
        return Ok(());
    };
    ensure!(
        nonce.to_string_lossy() == journal.nonce && journal.attempted,
        "Unexpected update launch token"
    );
    ensure!(
        manifest::hash_file(&layout.app())? == journal.new_sha256,
        "Installed update changed before launch verification"
    );
    atomic_write(&layout.healthy(), journal.nonce.as_bytes())
}

pub fn launch() -> Result<i32> {
    let executable = std::env::current_exe()?;
    let root = executable
        .parent()
        .context("Launcher directory is unavailable")?;
    let trust = crate::trust()?;
    let portable = platform::portable_root(&trust, &executable);
    let layout = if trust.validate().is_ok() {
        portable.ok().map(|root| Layout::new(&root)).transpose()?
    } else {
        None
    };
    let _supervisor = layout
        .as_ref()
        .map(|l| l.lock("launcher.lock", false))
        .transpose()
        .context("Freym is already running through its launcher")?;
    let mut nonce = None;
    if let Some(layout) = &layout {
        let _running = layout.running_lock().context("Freym is already running")?;
        let _download = layout.download_lock()?;
        layout.cleanup_interrupted()?;
        recover(layout, &trust)?;
        // Also finish a download left by a direct app launch, without any
        // network dependency in the launcher/startup path.
        if crate::worker::preference_enabled().unwrap_or(false)
            && !layout.journal().exists()
            && let Err(error) = install_ready(layout, &trust)
        {
            layout.record_error(&error);
        }
        nonce = begin_launch(layout)?;
    }
    let mut command = std::process::Command::new(root.join(platform::app_name()));
    command
        .args(std::env::args_os().skip(1))
        .env_remove("FREYM_UPDATE_HEALTH");
    if let Some(nonce) = nonce {
        command.env("FREYM_UPDATE_HEALTH", nonce);
    }
    let result = command.status();
    if let Some(layout) = &layout {
        // No waits or forced termination: if another direct instance is alive,
        // leave the transaction for the next launch.
        if let (Ok(_running), Ok(_download)) = (layout.running_lock(), layout.download_lock()) {
            layout.cleanup_interrupted()?;
            recover(layout, &trust)?;
            if crate::worker::preference_enabled().unwrap_or(false)
                && !layout.journal().exists()
                && let Err(error) = install_ready(layout, &trust)
            {
                layout.record_error(&error);
            }
        }
    }
    Ok(result?.code().unwrap_or(1))
}
