//! Filesystem and event bridge. All blocking methods run on background workers.
use crate::state::Preferences;
use frame_contracts::*;
use frame_runtime::recording_control::{EngineEventSink, RecordingControl};
use frame_settings::persistence::SettingsStore;
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct WakeSink {
    pub sender: async_channel::Sender<()>,
    pub error: Mutex<Option<String>>,
    pub warnings: Mutex<Vec<EngineWarning>>,
    pub replay_save_finished: AtomicBool,
}

pub(crate) fn error_details(error: &RecorderError) -> String {
    format!(
        "{}\n{}\nCode: {:?}\nRecovery: {:?}",
        error.message, error.action, error.code, error.recovery
    )
}

impl EngineEventSink for WakeSink {
    fn publish(&self, event: EventEnvelope) {
        match event.event {
            EngineEvent::ReplaySaved(_) => {
                self.replay_save_finished.store(true, Ordering::Release);
            }
            EngineEvent::ReplaySaveFailed(e) => {
                *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(error_details(&e));
                self.replay_save_finished.store(true, Ordering::Release);
            }
            EngineEvent::Error(e) => {
                *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(error_details(&e))
            }
            EngineEvent::Warning(warning) => {
                let mut warnings = self.warnings.lock().unwrap_or_else(|p| p.into_inner());
                if warnings.len() == 32 {
                    warnings.remove(0);
                }
                warnings.push(warning);
            }
            _ => {}
        }
        // Coalescing wakes cannot block a capture or media worker. The UI reads the
        // controller's coherent published state after every wake, so no sequence gap.
        let _ = self.sender.try_send(());
    }
}
pub struct Services {
    pub control: Arc<RecordingControl>,
    pub sink: Arc<WakeSink>,
    pub store: SettingsStore,
    pub config: PathBuf,
    pub recovery: frame_runtime::recovery::RecoveryJobs,
    failure_sequence: Mutex<u32>,
}
impl Services {
    pub(crate) fn save_recording_failure(
        &self,
        snapshot: &EngineSnapshot,
        request: Option<&RecordingRequest>,
    ) -> Result<(), String> {
        let RecorderState::Failed(error) = &snapshot.state else {
            return Ok(());
        };
        let mut sequence = self
            .failure_sequence
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if snapshot.sequence < *sequence {
            return Ok(());
        }
        let report = serde_json::json!({
            "unixTimeMs": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
            "sessionId": snapshot.session_id,
            "error": error,
            "request": request,
            "metrics": snapshot.metrics,
            "planDecisions": snapshot.plan_decisions,
        });
        atomic_write(
            &self.config.join("last-recording-error.json"),
            &serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )?;
        *sequence = snapshot.sequence;
        Ok(())
    }
    pub fn save_revisioned(
        &self,
        base: Option<frame_settings::model::SettingsDocument>,
        draft: Option<frame_settings::model::SettingsDocument>,
        prefs: &Preferences,
        snapshot: EngineSnapshot,
    ) -> Result<Option<frame_settings::model::SettingsDocument>, String> {
        use frame_settings::{
            capabilities::SettingsCapabilities, persistence::SettingsErrorKind,
            validation::ApplyTiming,
        };
        std::fs::create_dir_all(&self.config).map_err(|e| e.to_string())?;
        let mut saved = None;
        if let Some(mut draft) = draft {
            let mut expected = base.as_ref().map_or(0, |d| d.revision);
            for attempt in 0..2 {
                let state = self.control.published().state;
                let changed = base
                    .as_ref()
                    .is_none_or(|b| b.configuration() != draft.configuration())
                    || !self.store.path().exists();
                let caps = if changed {
                    frame_runtime::settings::discover(&draft.configuration()?, &state)
                        .map_err(|e| e.message)?
                } else {
                    SettingsCapabilities::from_snapshot(snapshot.clone())
                };
                let state = self.control.published().state;
                match self
                    .store
                    .commit(&draft, expected, &caps, &state, ApplyTiming::Now)
                {
                    Ok((document, _)) => {
                        saved = Some(document);
                        break;
                    }
                    Err(e) if e.kind == SettingsErrorKind::Conflict && attempt == 0 => {
                        let current = self
                            .store
                            .load_document()
                            .map_err(|e| e.to_string())?
                            .ok_or("Settings were removed by another writer. Reload settings.")?;
                        draft = frame_settings::model::SettingsDocument::rebase_edit(
                            base.as_ref().ok_or(
                                "Settings were created by another writer. Reload settings.",
                            )?,
                            &draft,
                            &current,
                        )?;
                        expected = current.revision;
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        self.save(None, prefs).map_err(|e| {
            if saved.is_some() {
                format!("Recording settings saved, but native preferences failed: {e}")
            } else {
                e
            }
        })?;
        Ok(saved)
    }
    pub fn new() -> (Arc<Self>, async_channel::Receiver<()>) {
        let (sender, receiver) = async_channel::bounded(1);
        let sink = Arc::new(WakeSink {
            sender,
            error: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
            replay_save_finished: AtomicBool::new(false),
        });
        let config = dirs_next::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("dev.frame.recorder");
        let store = SettingsStore::in_directory(&config);
        (
            Arc::new(Self {
                control: Arc::new(RecordingControl::new(sink.clone())),
                sink,
                store,
                config,
                recovery: Default::default(),
                failure_sequence: Mutex::new(0),
            }),
            receiver,
        )
    }
    pub fn default_output(&self) -> Result<String, String> {
        let path = dirs_next::video_dir()
            .ok_or("The Videos folder is unavailable. Choose an output folder.")?
            .join("FREYM");
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    }
    pub fn preferences(&self) -> Result<Preferences, String> {
        match std::fs::read(self.config.join("native-ui.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("Could not read native preferences: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Preferences::default()),
            Err(e) => Err(e.to_string()),
        }
    }
    pub fn save(
        &self,
        request: Option<&RecordingRequest>,
        prefs: &Preferences,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&self.config).map_err(|e| e.to_string())?;
        if let Some(request) = request {
            self.store.save(request).map_err(|e| e.to_string())?;
        }
        let bytes = serde_json::to_vec_pretty(prefs).map_err(|e| e.to_string())?;
        atomic_write(&self.config.join("native-ui.json"), &bytes)
    }
    pub fn shortcut_bindings(&self) -> Result<ShortcutBindings, String> {
        match std::fs::read(self.config.join("shortcuts.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| format!("Could not read shortcuts: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ShortcutBindings::default()),
            Err(e) => Err(e.to_string()),
        }
    }
    pub fn save_shortcuts(&self, bindings: &ShortcutBindings) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(bindings).map_err(|e| e.to_string())?;
        atomic_write(&self.config.join("shortcuts.json"), &bytes)
    }
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or("Preferences directory is unavailable.")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .ok_or("Preferences filename is unavailable.")?
        .to_string_lossy();
    let temp = parent.join(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|e: std::io::Error| e.to_string())
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    #[test]
    fn finishing_failure_details_and_settings_survive_disk_reload_without_touching_media() {
        let directory = std::env::temp_dir().join(format!(
            "freym-finishing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let (services, _) = Services::new();
        let mut services = Arc::try_unwrap(services).ok().unwrap();
        services.config = directory.clone();
        services.store = SettingsStore::in_directory(&directory);
        let media = directory.join("original.mkv");
        std::fs::write(&media, b"preserved original").unwrap();
        let mut snapshot = crate::state::AppModel::default().snapshot;
        let error = RecorderError::invalid("Native Start failed: HRESULT 0x80070005");
        snapshot.state = RecorderState::Failed(error.clone());
        snapshot.sequence = 10;
        services
            .save_recording_failure(&snapshot, Some(&request()))
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("last-recording-error.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_value::<RecorderError>(report["error"].clone()).unwrap(),
            error
        );
        assert!(
            error_details(&error).contains("Code:") && error_details(&error).contains("Recovery:")
        );
        assert_eq!(std::fs::read(&media).unwrap(), b"preserved original");
        snapshot.sequence = 9;
        snapshot.state = RecorderState::Failed(RecorderError::invalid("Older failure"));
        services.save_recording_failure(&snapshot, None).unwrap();
        let kept: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("last-recording-error.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            kept["error"], report["error"],
            "background completion order must not replace a newer failure"
        );
        let base = services
            .store
            .save_document(&SettingsDocument::new(request()), 0)
            .unwrap();
        let prefs = Preferences {
            countdown: 17,
            reduced_motion: true,
            ..Default::default()
        };
        snapshot.state = RecorderState::Idle;
        let saved = services
            .save_revisioned(Some(base.clone()), Some(base), &prefs, snapshot)
            .unwrap()
            .unwrap();
        assert_eq!(services.store.load_document().unwrap(), Some(saved));
        assert_eq!(services.preferences().unwrap().countdown, 17);
        assert!(services.preferences().unwrap().reduced_motion);
        assert_eq!(std::fs::read(&media).unwrap(), b"preserved original");
        drop(services);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn revision_shortcuts_persist_clear_restore_and_reload_without_defaults() {
        let folder = std::env::temp_dir().join(format!(
            "freym-shortcut-persistence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (mut services, _) = Services::new();
        Arc::get_mut(&mut services).unwrap().config = folder.clone();
        let path = folder.join("shortcuts.json");
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(
            services.shortcut_bindings().unwrap(),
            ShortcutBindings::default()
        );
        let saved = ShortcutBindings {
            start_stop: "Ctrl+Shift+R".into(),
            pause_resume: "Alt+F9".into(),
            save_replay: String::new(),
        };
        services.save_shortcuts(&saved).unwrap();
        let (mut reopened, _) = Services::new();
        Arc::get_mut(&mut reopened).unwrap().config = folder;
        assert_eq!(reopened.shortcut_bindings().unwrap(), saved);
        services
            .save_shortcuts(&ShortcutBindings::default())
            .unwrap();
        assert_eq!(
            reopened.shortcut_bindings().unwrap(),
            ShortcutBindings::default()
        );
        services.save_shortcuts(&saved).unwrap();
        assert_eq!(reopened.shortcut_bindings().unwrap(), saved);
        std::fs::remove_file(path).unwrap();
    }
    use frame_settings::model::{SettingsDocument, SettingsMode};
    fn request() -> RecordingRequest {
        RecordingRequest {
            target: CaptureTarget::Display(DisplayId("fixture".into())),
            fps_mode: FpsMode::AutoMatchSource,
            resolution: Resolution::Native,
            quality: Quality::Standard,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_encoding: Default::default(),
            audio_routes: vec![],
            camera: None,

            show_cursor: true,
            output: OutputSettings {
                directory: std::env::temp_dir().to_string_lossy().into_owned(),
                file_stem: "fixture".into(),
            },
        }
    }
    #[test]
    fn dock_layout_persists_without_writing_recording_settings() {
        use crate::state::workspace::{Edge, Panel};
        let directory = std::env::temp_dir().join(format!(
            "freym-dock-prefs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (services, _) = Services::new();
        let mut services = Arc::try_unwrap(services).ok().unwrap();
        services.config = directory.clone();
        services.store = SettingsStore::in_directory(&directory);
        let mut prefs = Preferences::default();
        assert!(
            prefs
                .workspace
                .dock(Panel::Audio, Panel::Preview, Edge::Right)
        );
        assert!(prefs.workspace.resize(&[], 550));
        services.save(None, &prefs).unwrap();
        assert_eq!(services.preferences().unwrap().workspace, prefs.workspace);
        assert!(!services.store.path().exists());
        assert_eq!(services.control.published().state, RecorderState::Idle);
        // Only this test's freshly created temporary directory is removed.
        std::fs::remove_file(directory.join("native-ui.json")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn native_mode_saves_share_a_revision_and_reapply_unrelated_concurrent_edits() {
        let directory = std::env::temp_dir().join(format!(
            "freym-ui-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let (services, _) = Services::new();
        let mut services = Arc::try_unwrap(services).ok().unwrap();
        services.config = directory.clone();
        services.store = SettingsStore::in_directory(&directory);
        let base = services
            .store
            .save_document(&SettingsDocument::new(request()), 0)
            .unwrap();
        let mut external_audio = base.clone();
        external_audio.modes.audio = SettingsMode::Advanced;
        let external_audio = services
            .store
            .save_document(&external_audio, base.revision)
            .unwrap();
        let mut video = base.clone();
        video.modes.video = SettingsMode::Advanced;
        let saved = services
            .save_revisioned(
                Some(base),
                Some(video),
                &Preferences::default(),
                crate::state::AppModel::default().snapshot,
            )
            .unwrap()
            .unwrap();
        assert_eq!(saved.revision, external_audio.revision + 1);
        assert_eq!(saved.modes.audio, SettingsMode::Advanced);
        assert_eq!(saved.modes.video, SettingsMode::Advanced);
        assert_eq!(services.store.load_document().unwrap().unwrap(), saved);
        assert_eq!(services.store.load().unwrap().unwrap(), request());
        assert!(directory.join("native-ui.json").exists());
        drop(services);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
