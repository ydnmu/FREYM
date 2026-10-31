//! One effective recording configuration, with presentation modes kept outside it.
use frame_contracts::*;
pub use frame_contracts::{AudioChannels, AudioCodec, AudioQuality, AudioSettings};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingsMode {
    #[default]
    Easy,
    Advanced,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsModes {
    pub video: SettingsMode,
    pub audio: SettingsMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordingConfiguration {
    /// Existing engine contract remains the sole owner of video and source routing.
    pub request: RecordingRequest,
    pub audio: AudioSettings,
}
impl From<RecordingRequest> for RecordingConfiguration {
    fn from(request: RecordingRequest) -> Self {
        let audio = request.audio_encoding;
        Self { request, audio }
    }
}
impl RecordingConfiguration {
    /// Admission boundary for existing engine consumers. Never discard unsupported audio.
    pub fn engine_request(&self) -> Result<RecordingRequest, RecorderError> {
        let mut request = self.request.for_capture();
        crate::validate_request(&request)?;
        crate::audio_admission::validate_audio_topology(&self.request.audio_routes)?;
        self.audio.validate()?;
        request.audio_encoding = self.audio;
        Ok(request)
    }
}

/// Explicit inheritance also permits clearing optional values, unlike Option<Option<T>>.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum Override<T> {
    #[default]
    Inherit,
    Set(T),
}
impl<T: Clone> Override<T> {
    fn apply(&self, target: &mut T) {
        if let Self::Set(value) = self {
            *target = value.clone();
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigurationOverrides {
    pub target: Override<CaptureTarget>,
    pub fps_mode: Override<FpsMode>,
    pub resolution: Override<Resolution>,
    pub quality: Override<Quality>,
    pub encoder: Override<EncoderPreference>,
    pub optimization: Override<Option<RecordingOptimization>>,
    pub audio_routes: Override<Vec<AudioRouteSelection>>,
    pub camera: Override<Option<CameraSelection>>,

    pub show_cursor: Override<bool>,
    pub output: Override<OutputSettings>,
    pub audio: Override<AudioSettings>,
}
impl ConfigurationOverrides {
    pub fn apply(&self, configuration: &mut RecordingConfiguration) {
        let r = &mut configuration.request;
        self.target.apply(&mut r.target);
        self.fps_mode.apply(&mut r.fps_mode);
        self.resolution.apply(&mut r.resolution);
        self.quality.apply(&mut r.quality);
        self.encoder.apply(&mut r.encoder);
        self.optimization.apply(&mut r.optimization);
        self.audio_routes.apply(&mut r.audio_routes);
        self.camera.apply(&mut r.camera);
        self.show_cursor.apply(&mut r.show_cursor);
        self.output.apply(&mut r.output);
        self.audio.apply(&mut configuration.audio);
    }

    pub fn between(base: &RecordingConfiguration, next: &RecordingConfiguration) -> Self {
        fn difference<T: Clone + PartialEq>(base: &T, next: &T) -> Override<T> {
            if base == next {
                Override::Inherit
            } else {
                Override::Set(next.clone())
            }
        }
        let b = &base.request;
        let n = &next.request;
        Self {
            target: difference(&b.target, &n.target),
            fps_mode: difference(&b.fps_mode, &n.fps_mode),
            resolution: difference(&b.resolution, &n.resolution),
            quality: difference(&b.quality, &n.quality),
            encoder: difference(&b.encoder, &n.encoder),
            optimization: difference(&b.optimization, &n.optimization),
            audio_routes: difference(&b.audio_routes, &n.audio_routes),
            camera: difference(&b.camera, &n.camera),

            show_cursor: difference(&b.show_cursor, &n.show_cursor),
            output: difference(&b.output, &n.output),
            audio: difference(&base.audio, &next.audio),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsProfile {
    pub name: String,
    pub overrides: ConfigurationOverrides,
}

/// Resolution order: defaults -> selected profile -> installation overrides.
/// Modes never choose a different configuration. revision is assigned by the store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsDocument {
    pub schema_version: u32,
    pub revision: u64,
    pub defaults: RecordingConfiguration,
    pub modes: SettingsModes,
    pub profiles: BTreeMap<String, SettingsProfile>,
    pub active_profile: Option<String>,
    pub overrides: ConfigurationOverrides,
}
impl SettingsDocument {
    /// Reapply a UI edit to a freshly loaded revision. Conflicting edits to the
    /// same field require review; unrelated fields and profiles remain intact.
    pub fn rebase_edit(base: &Self, edited: &Self, current: &Self) -> Result<Self, String> {
        let b = base.configuration()?;
        let e = edited.configuration()?;
        let mut c = current.configuration()?;
        macro_rules! field {
            ($($path:ident).+) => {
                if b.$($path).+ != e.$($path).+ {
                    if c.$($path).+ != b.$($path).+ && c.$($path).+ != e.$($path).+ {
                        return Err(concat!("Settings conflict in ", stringify!($($path).+), ". Reload and reapply this edit.").into());
                    }
                    c.$($path).+ = e.$($path).+.clone();
                }
            };
        }
        field!(request.target);
        field!(request.fps_mode);
        field!(request.resolution);
        field!(request.quality);
        field!(request.encoder);
        field!(request.optimization);
        field!(request.audio_routes);
        field!(request.camera);
        field!(request.show_cursor);
        field!(request.output);
        field!(audio);
        let mut result = current.clone();
        result.set_configuration(&c)?;
        for (old, edit, latest) in [
            (
                base.modes.video,
                edited.modes.video,
                &mut result.modes.video,
            ),
            (
                base.modes.audio,
                edited.modes.audio,
                &mut result.modes.audio,
            ),
        ] {
            if old != edit {
                if *latest != old && *latest != edit {
                    return Err("Settings presentation conflict. Reload and reapply.".into());
                }
                *latest = edit;
            }
        }
        if base.profiles != edited.profiles
            || base.active_profile != edited.active_profile
            || base.defaults != edited.defaults
        {
            return Err("Profile/default edits require a fresh settings revision.".into());
        }
        Ok(result)
    }
    /// The host supplies defaults with its real target and output folder.
    pub fn new(defaults: impl Into<RecordingConfiguration>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            revision: 0,
            defaults: defaults.into(),
            modes: Default::default(),
            profiles: BTreeMap::new(),
            active_profile: None,
            overrides: Default::default(),
        }
    }
    pub fn profile_configuration(&self) -> Result<RecordingConfiguration, String> {
        let mut result = self.defaults.clone();
        if let Some(id) = &self.active_profile {
            self.profiles
                .get(id)
                .ok_or_else(|| format!("Profile {id} does not exist."))?
                .overrides
                .apply(&mut result);
        }
        Ok(result)
    }
    pub fn configuration(&self) -> Result<RecordingConfiguration, String> {
        let mut result = self.profile_configuration()?;
        self.overrides.apply(&mut result);
        Ok(result)
    }
    pub fn set_configuration(&mut self, next: &RecordingConfiguration) -> Result<(), String> {
        self.overrides = ConfigurationOverrides::between(&self.profile_configuration()?, next);
        Ok(())
    }
    /// Switching profiles deliberately clears installation overrides, so the selected
    /// profile takes effect. Defaults and every profile retain their advanced values.
    pub fn select_profile(&mut self, id: Option<&str>) -> Result<(), String> {
        if let Some(id) = id
            && !self.profiles.contains_key(id)
        {
            return Err(format!("Profile {id} does not exist."));
        }
        self.active_profile = id.map(str::to_owned);
        self.overrides = Default::default();
        Ok(())
    }
    pub fn capture_profile(&mut self, id: String, name: String) -> Result<(), String> {
        if id.trim().is_empty() || name.trim().is_empty() {
            return Err("Profile ID and name must be nonempty.".into());
        }
        let overrides = ConfigurationOverrides::between(&self.defaults, &self.configuration()?);
        self.profiles
            .insert(id, SettingsProfile { name, overrides });
        Ok(())
    }
    pub fn reset_overrides(&mut self) {
        self.overrides = Default::default();
    }
}

/// A projection of actual settings, never a remembered Easy-mode selection.
pub fn video_easy_summary(configuration: &RecordingConfiguration) -> RecordingPreset {
    let r = &configuration.request;
    let Some(o) = &r.optimization else {
        return RecordingPreset::Custom;
    };
    if r.encoder != EncoderPreference::Auto
        || r.resolution != Resolution::Native
        || r.fps_mode != FpsMode::AutoMatchSource
        || o.codec != RecordingCodec::Auto
        || o.bitrate_bps.is_some()
        || !o.encoder_overrides.is_empty()
        || !o.automatic_quality
    {
        return RecordingPreset::Custom;
    }
    o.preset
}
pub fn set_video_easy_preset(configuration: &mut RecordingConfiguration, preset: RecordingPreset) {
    let o = configuration
        .request
        .optimization
        .get_or_insert_with(Default::default);
    o.preset = preset;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEasySummary {
    Automatic,
    Custom,
}
pub fn audio_easy_summary(configuration: &RecordingConfiguration) -> AudioEasySummary {
    if configuration.audio == AudioSettings::default()
        && configuration.request.audio_routes.iter().all(|r| {
            matches!(r.route, AudioRoute::Ignore | AudioRoute::MainMix)
                && r.controls == AudioSourceControls::default()
        })
    {
        AudioEasySummary::Automatic
    } else {
        AudioEasySummary::Custom
    }
}
