//! Shared legacy request and versioned configuration document storage.
use super::*;
use crate::capabilities::SettingsCapabilities;
use crate::validation::{
    ApplyTiming, ChangeEffect, change_effect, validate_configuration, validate_document,
};
use frame_contracts::RecorderState;

// A cooperative cross-process lock prevents lost updates across GPUI editors and hosts.
// A stale lock is a visible conflict, never silently removed while another process may own it.
struct WriteLock(PathBuf);
impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl SettingsStore {
    pub fn backup_path(&self) -> PathBuf {
        self.path.with_extension("json.previous")
    }
    fn lock(&self) -> Result<WriteLock, SettingsError> {
        let path = self.path.with_extension("json.lock");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                self.error(
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        SettingsErrorKind::Conflict
                    } else {
                        SettingsErrorKind::Write
                    },
                    format!("Could not acquire settings write lock: {e}"),
                )
            })?;
        let guard = WriteLock(path);
        writeln!(file, "{}", std::process::id())
            .map_err(|e| self.error(SettingsErrorKind::Write, e.to_string()))?;
        Ok(guard)
    }

    fn decode(&self, bytes: &[u8]) -> Result<SettingsDocument, SettingsError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|e| self.error(SettingsErrorKind::Corrupt, e.to_string()))?;
        let object = value.as_object().ok_or_else(|| {
            self.error(
                SettingsErrorKind::Corrupt,
                "Settings must be an object.".into(),
            )
        })?;
        let version = match object.get("schemaVersion") {
            None => 1,
            Some(v) => v.as_u64().filter(|v| *v != 0).ok_or_else(|| {
                self.error(
                    SettingsErrorKind::Corrupt,
                    "schemaVersion must be a positive integer.".into(),
                )
            })?,
        };
        if version > u64::from(SCHEMA_VERSION) {
            return Err(self.error(SettingsErrorKind::UnsupportedVersion, format!("schemaVersion {version} is newer than {SCHEMA_VERSION}; settings were left intact.")));
        }
        let document = match version {
            1 => {
                let old: Version1Document = serde_json::from_value(value)
                    .map_err(|e| self.error(SettingsErrorKind::Corrupt, e.to_string()))?;
                SettingsDocument::new(RecordingRequest::from(old.request))
            }
            2 => {
                let old: Version2Document = serde_json::from_value(value)
                    .map_err(|e| self.error(SettingsErrorKind::Corrupt, e.to_string()))?;
                SettingsDocument::new(old.request)
            }
            3 => {
                let mut migrated: SettingsDocument = serde_json::from_value(value)
                    .map_err(|e| self.error(SettingsErrorKind::Corrupt, e.to_string()))?;
                migrated.schema_version = SCHEMA_VERSION;
                migrated
            }
            4 => serde_json::from_value(value)
                .map_err(|e| self.error(SettingsErrorKind::Corrupt, e.to_string()))?,
            _ => {
                return Err(self.error(SettingsErrorKind::Corrupt, "Unknown legacy schema.".into()));
            }
        };
        validate_document(&document)
            .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
        Ok(document)
    }
    fn read_bytes(&self) -> Result<Option<Vec<u8>>, SettingsError> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.error(SettingsErrorKind::Read, e.to_string())),
        }
    }
    /// Migrates in memory only. A successful save writes the current schema.
    pub fn load_document(&self) -> Result<Option<SettingsDocument>, SettingsError> {
        self.read_bytes()?
            .map(|bytes| self.decode(&bytes))
            .transpose()
    }
    /// Backward-compatible native settings API, resolving the same configuration as profiles.
    pub fn load(&self) -> Result<Option<RecordingRequest>, SettingsError> {
        self.load_document()?
            .map(|doc| {
                let configuration = doc
                    .configuration()
                    .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
                configuration
                    .audio
                    .validate()
                    .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e.message))?;
                let mut request = configuration.request;
                request.audio_encoding = configuration.audio;
                Ok(request)
            })
            .transpose()
    }
    /// Legacy request saves preserve modes, profiles and audio settings. They affect
    /// the next recording; no engine plan is changed by filesystem persistence.
    pub fn save(&self, request: &RecordingRequest) -> Result<(), SettingsError> {
        let _lock = self.lock()?;
        let previous = self.read_bytes()?;
        let mut document = previous
            .as_ref()
            .map(|bytes| self.decode(bytes))
            .transpose()?
            .unwrap_or_else(|| SettingsDocument::new(request.clone()));
        let mut configuration = document
            .configuration()
            .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
        configuration.request = request.clone();
        // Existing documents own audio through RecordingConfiguration.audio.
        // Older request-only clients must not reset it by omitting audioEncoding.
        document
            .set_configuration(&configuration)
            .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
        self.write_document(document, previous.as_deref())?;
        Ok(())
    }
    /// Storage-only CAS. UI hosts should use commit for admission and live change checks.
    /// The supplied revision is the revision loaded by the caller (zero for a new store).
    pub fn save_document(
        &self,
        document: &SettingsDocument,
        expected_revision: u64,
    ) -> Result<SettingsDocument, SettingsError> {
        let _lock = self.lock()?;
        let previous = self.read_bytes()?;
        self.check_revision(previous.as_deref(), expected_revision)?;
        let mut document = document.clone();
        document.revision = expected_revision;
        self.write_document(document, previous.as_deref())
    }
    fn check_revision(&self, bytes: Option<&[u8]>, expected: u64) -> Result<(), SettingsError> {
        let revision = bytes
            .map(|bytes| self.decode(bytes))
            .transpose()?
            .map_or(0, |doc| doc.revision);
        if revision != expected {
            return Err(self.error(SettingsErrorKind::Conflict, format!("Settings changed from revision {expected} to {revision}. Reload and reapply your edit.")));
        }
        Ok(())
    }
    /// Persist a validated UI draft. Presentation-only edits can always be saved,
    /// including when the currently selected device has been unplugged.
    pub fn commit(
        &self,
        document: &SettingsDocument,
        expected_revision: u64,
        caps: &SettingsCapabilities,
        state: &RecorderState,
        timing: ApplyTiming,
    ) -> Result<(SettingsDocument, ChangeEffect), SettingsError> {
        let _lock = self.lock()?;
        let previous = self.read_bytes()?;
        self.check_revision(previous.as_deref(), expected_revision)?;
        let old = previous
            .as_ref()
            .map(|bytes| self.decode(bytes))
            .transpose()?
            .unwrap_or_else(|| SettingsDocument::new(document.defaults.clone()));
        let effect = change_effect(&old, document, state, timing)
            .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
        // A new store must be admitted even when the initial draft equals defaults.
        if effect != ChangeEffect::PresentationOnly || previous.is_none() {
            let report = validate_configuration(
                &document
                    .configuration()
                    .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?,
                caps,
            );
            if !report.is_valid() {
                return Err(self.error(
                    SettingsErrorKind::InvalidSelection,
                    report
                        .issues
                        .iter()
                        .map(|i| format!("{}: {}", i.field, i.message))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            }
        }
        let mut document = document.clone();
        document.revision = expected_revision;
        Ok((self.write_document(document, previous.as_deref())?, effect))
    }

    /// Explicit rollback only. Corrupt/newer files never trigger automatic replacement.
    /// Restoring a backup also saves the displaced bytes, so the operation is reversible.
    /// While recording, rollback is limited to NextRecording by change_effect.
    pub fn rollback(
        &self,
        expected_revision: u64,
        state: &RecorderState,
        timing: ApplyTiming,
    ) -> Result<SettingsDocument, SettingsError> {
        let _lock = self.lock()?;
        let previous = self.read_bytes()?;
        self.check_revision(previous.as_deref(), expected_revision)?;
        let old = previous
            .as_ref()
            .map(|bytes| self.decode(bytes))
            .transpose()?;
        let bytes = fs::read(self.backup_path()).map_err(|e| {
            self.error(
                SettingsErrorKind::Read,
                format!("Could not read rollback copy: {e}"),
            )
        })?;
        let mut backup = self.decode(&bytes)?;
        if let Some(old) = &old {
            change_effect(old, &backup, state, timing)
                .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
        }
        backup.revision = expected_revision;
        self.write_document(backup, previous.as_deref())
    }
    /// Recover a corrupt primary using exact bytes reviewed by the caller as a CAS
    /// token. Keep the corrupt bytes in the rollback copy; never recover over a newer
    /// schema or a valid primary. Recovery cannot affect an active engine plan.
    pub fn recover_backup(
        &self,
        expected_corrupt_bytes: &[u8],
        state: &RecorderState,
        timing: ApplyTiming,
    ) -> Result<SettingsDocument, SettingsError> {
        let _lock = self.lock()?;
        let previous = self.read_bytes()?.ok_or_else(|| {
            self.error(
                SettingsErrorKind::Read,
                "No primary settings file to recover.".into(),
            )
        })?;
        if previous != expected_corrupt_bytes {
            return Err(self.error(
                SettingsErrorKind::Conflict,
                "Primary settings changed before recovery. Read it again.".into(),
            ));
        }
        match self.decode(&previous) {
            Err(e)
                if matches!(
                    e.kind,
                    SettingsErrorKind::Corrupt | SettingsErrorKind::InvalidSelection
                ) => {}
            Err(e) => return Err(e),
            Ok(_) => {
                return Err(self.error(
                    SettingsErrorKind::InvalidSelection,
                    "Primary settings are valid; use revision-checked rollback.".into(),
                ));
            }
        }
        if !matches!(state, RecorderState::Idle | RecorderState::Failed(_))
            && timing == ApplyTiming::Now
        {
            return Err(self.error(
                SettingsErrorKind::InvalidSelection,
                "Recover settings for the next recording while an engine session is active.".into(),
            ));
        }
        let bytes = fs::read(self.backup_path()).map_err(|e| {
            self.error(
                SettingsErrorKind::Read,
                format!("Could not read rollback copy: {e}"),
            )
        })?;
        let mut backup = self.decode(&bytes)?;
        // The displaced primary followed this backup. Skip its revision so drafts
        // loaded before corruption cannot silently overwrite the recovered file.
        backup.revision = backup.revision.checked_add(1).ok_or_else(|| {
            self.error(
                SettingsErrorKind::Write,
                "Settings revision overflow.".into(),
            )
        })?;
        if let Some(displaced_revision) = serde_json::from_slice::<serde_json::Value>(&previous)
            .ok()
            .and_then(|value| value.get("revision").and_then(|v| v.as_u64()))
        {
            backup.revision = backup.revision.max(displaced_revision);
        }
        self.write_document(backup, Some(&previous))
    }
    fn write_document(
        &self,
        mut document: SettingsDocument,
        previous: Option<&[u8]>,
    ) -> Result<SettingsDocument, SettingsError> {
        validate_document(&document)
            .map_err(|e| self.error(SettingsErrorKind::InvalidSelection, e))?;
        document.revision = document.revision.checked_add(1).ok_or_else(|| {
            self.error(
                SettingsErrorKind::Write,
                "Settings revision overflow.".into(),
            )
        })?;
        let mut bytes = serde_json::to_vec_pretty(&document)
            .map_err(|e| self.error(SettingsErrorKind::Write, e.to_string()))?;
        bytes.push(b'\n');
        if let Some(previous) = previous {
            self.replace_bytes(&self.backup_path(), previous)?;
        }
        self.replace_bytes(&self.path, &bytes)?;
        Ok(document)
    }
    fn replace_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), SettingsError> {
        let directory = path.parent().ok_or_else(|| {
            self.error(
                SettingsErrorKind::Write,
                "Settings path has no parent directory.".into(),
            )
        })?;
        let sequence = SAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(
            "{}.{}.{sequence}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let mut created = false;
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            created = true;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)
        })();
        if let Err(e) = result {
            if created {
                let _ = fs::remove_file(&temporary);
            }
            return Err(self.error(
                SettingsErrorKind::Write,
                format!("Could not atomically replace {}: {e}", path.display()),
            ));
        }
        Ok(())
    }
}
