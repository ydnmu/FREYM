//! Versioned local recording settings (FRAME-0207).
//!
//! The stored document holds the same canonical selections a
//! [`frame_contracts::RecordingRequest`] already carries; no second settings model is
//! introduced here. The file is JSON with an explicit `schemaVersion`, it is replaced
//! atomically through a temporary file in the same directory, a corrupt document is a
//! visible error rather than a silent reset to defaults, and an older document is migrated
//! by an explicit step. A version written by a newer build is refused instead of being
//! guessed at.
//!
//! Reading and writing are the only things this module does: it never builds a plan and
//! never opens a recording. A live session holds its own frozen plan, so a settings file
//! that changes on disk cannot alter a recording that is already running.

use crate::model::{SCHEMA_VERSION, SettingsDocument};
use frame_contracts::{
    CaptureTarget, EncoderPreference, FpsMode, OutputSettings, Quality, RecordingRequest,
    Resolution,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The schema version this build writes.
///
/// Version 1 was written before audio routing was part of a recording request: it has no
/// `audioRoutes` member, and migrating it adds an empty route list — no audio source was
/// routable then, so the migrated request records exactly what the old document meant.
pub const SETTINGS_SCHEMA_VERSION: u32 = SCHEMA_VERSION;
/// The settings document name inside the application's own directory.
pub const SETTINGS_FILE_NAME: &str = "recording-settings.json";

/// Why a settings document could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsErrorKind {
    /// The file exists but could not be read.
    Read,
    /// The file is not a settings document this build can read at all.
    Corrupt,
    /// The document was written by a newer build.
    UnsupportedVersion,
    /// The document is readable but holds a request that is not valid.
    InvalidSelection,
    /// The document could not be written or replaced.
    Write,
    /// Another editor saved after this draft was loaded, or holds the write lock.
    Conflict,
}

/// One visible settings failure, naming the file it is about.
#[derive(Debug)]
pub struct SettingsError {
    pub path: PathBuf,
    pub kind: SettingsErrorKind,
    pub detail: String,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({:?}): {}",
            self.path.display(),
            self.kind,
            self.detail
        )
    }
}

impl std::error::Error for SettingsError {}

/// The document this build writes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Version2Document {
    schema_version: u32,
    request: RecordingRequest,
}

/// The version 1 document: the request before audio routing existed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Version1Document {
    request: Version1Request,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Version1Request {
    target: CaptureTarget,
    fps_mode: FpsMode,
    resolution: Resolution,
    quality: Quality,
    encoder: EncoderPreference,
    output: OutputSettings,
}

impl From<Version1Request> for RecordingRequest {
    fn from(legacy: Version1Request) -> Self {
        Self {
            target: legacy.target,
            fps_mode: legacy.fps_mode,
            resolution: legacy.resolution,
            quality: legacy.quality,
            encoder: legacy.encoder,
            optimization: None,
            // Migration step: version 1 had no audio routing, so the migrated request
            // routes nothing rather than inventing a source.
            audio_encoding: Default::default(),
            audio_routes: Vec::new(),
            camera: None,

            show_cursor: true,
            output: legacy.output,
        }
    }
}

/// The settings file of one installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// A store at one exact path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A store at [`SETTINGS_FILE_NAME`] inside one already existing directory.
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        Self {
            path: directory.as_ref().join(SETTINGS_FILE_NAME),
        }
    }

    /// The path this store reads and replaces.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn error(&self, kind: SettingsErrorKind, detail: String) -> SettingsError {
        SettingsError {
            path: self.path.clone(),
            kind,
            detail,
        }
    }
}

#[path = "document_store.rs"]
mod document_store;

#[cfg(test)]
mod tests {
    use super::*;
    use frame_contracts::{AudioRoute, AudioRouteSelection, AudioSourceId, DisplayId};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "frame-0207-settings-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn request() -> RecordingRequest {
        RecordingRequest {
            target: CaptureTarget::Display(DisplayId("display-1".into())),
            fps_mode: FpsMode::Fixed(119.88),
            resolution: Resolution::Native,
            quality: Quality::Maximum,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_encoding: Default::default(),
            audio_routes: vec![AudioRouteSelection {
                controls: Default::default(),
                muted: false,
                source_id: AudioSourceId("microphone-1".into()),
                route: AudioRoute::MainMix,
            }],
            camera: None,

            show_cursor: true,
            output: OutputSettings {
                directory: "recordings".into(),
                file_stem: "session".into(),
            },
        }
    }

    #[test]
    fn selections_round_trip_and_a_version_1_document_migrates() {
        let directory = temporary_directory();
        let store = SettingsStore::in_directory(&directory);
        assert_eq!(store.load().unwrap(), None, "no file yet is not an error");

        store.save(&request()).unwrap();
        assert_eq!(store.load().unwrap(), Some(request()));
        let mut without_cursor = request();
        without_cursor.show_cursor = false;
        store.save(&without_cursor).unwrap();
        assert_eq!(store.load().unwrap(), Some(without_cursor));
        let mut optimized = request();
        optimized.optimization = Some(frame_contracts::RecordingOptimization {
            preset: frame_contracts::RecordingPreset::Gaming,
            codec: frame_contracts::RecordingCodec::Av1,
            automatic_quality: false,
            bitrate_bps: Some(17_000_000),
            encoder_overrides: Default::default(),
        });
        store.save(&optimized).unwrap();
        assert_eq!(store.load().unwrap(), Some(optimized));
        let mut older_v2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        older_v2["defaults"]["request"]
            .as_object_mut()
            .unwrap()
            .remove("showCursor");
        fs::write(store.path(), serde_json::to_vec(&older_v2).unwrap()).unwrap();
        assert!(store.load().unwrap().unwrap().show_cursor);
        store.save(&request()).unwrap();
        let written = fs::read_to_string(store.path()).unwrap();
        assert!(written.contains("\"schemaVersion\": 4"));
        assert!(written.contains("\"audioRoutes\""));
        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            2,
            "no temporary file survives a successful save"
        );

        // A version 1 document is the request before audio routing existed.
        fs::write(
            store.path(),
            r#"{
  "request": {
    "target": { "kind": "display", "value": "display-1" },
    "fpsMode": { "kind": "autoMatchSource" },
    "resolution": { "kind": "native" },
    "quality": "standard",
    "encoder": { "kind": "auto" },
    "output": { "directory": "recordings", "fileStem": "session" }
  }
}"#,
        )
        .unwrap();
        let migrated = store.load().unwrap().unwrap();
        assert_eq!(migrated.quality, Quality::Standard);
        assert_eq!(migrated.fps_mode, FpsMode::AutoMatchSource);
        assert!(migrated.audio_routes.is_empty());
        assert!(migrated.show_cursor);
        assert!(migrated.optimization.is_none());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn corrupt_invalid_and_newer_documents_are_visible_errors() {
        let directory = temporary_directory();
        let store = SettingsStore::in_directory(&directory);

        fs::write(store.path(), "{ not json").unwrap();
        assert_eq!(
            store.load().unwrap_err().kind,
            SettingsErrorKind::Corrupt,
            "a corrupt document is never read as \"no settings\""
        );
        fs::write(store.path(), "[1, 2, 3]").unwrap();
        assert_eq!(store.load().unwrap_err().kind, SettingsErrorKind::Corrupt);
        fs::write(store.path(), r#"{"schemaVersion": 5, "request": {}}"#).unwrap();
        assert_eq!(
            store.load().unwrap_err().kind,
            SettingsErrorKind::UnsupportedVersion
        );
        fs::write(
            store.path(),
            r#"{"schemaVersion": 2, "request": {"target": {"kind": "display", "value": ""},
                "fpsMode": {"kind": "autoMatchSource"}, "resolution": {"kind": "native"},
                "quality": "high", "encoder": {"kind": "auto"}, "audioRoutes": [],
                "output": {"directory": "recordings", "fileStem": "session"}}}"#,
        )
        .unwrap();
        assert_eq!(
            store.load().unwrap_err().kind,
            SettingsErrorKind::InvalidSelection
        );

        // The failed loads never touched the document, and a save into a missing
        // directory is an explicit error rather than a silently created tree.
        let missing = SettingsStore::in_directory(directory.join("absent"));
        assert_eq!(
            missing.save(&request()).unwrap_err().kind,
            SettingsErrorKind::Write
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}
