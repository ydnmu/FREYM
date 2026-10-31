//! Exact render-endpoint WASAPI loopback. This captures the selected output mix,
//! including Windows sounds. It does not require a Stereo Mix capture device.
use frame_contracts::*;

pub(crate) const REST_PREFIX: &str = "wasapi-rest:";

pub(crate) const PREFIX: &str = "wasapi-loopback:";

pub(crate) fn endpoint(id: &AudioSourceId) -> Option<&str> {
    id.0.strip_prefix(PREFIX)
        .filter(|s| !s.is_empty() && !s.contains('\0') && s.encode_utf16().count() <= 4096)
}

pub(crate) fn enumerate() -> Result<Vec<AudioSource>, RecorderError> {
    Ok(crate::microphone::native::enumerate_render_endpoints()?
        .into_iter()
        .map(|e| AudioSource {
            controls: Default::default(),
            capture_identity: None,
            id: AudioSourceId(format!("{PREFIX}{}", e.endpoint_id)),
            display_name: format!(
                "Desktop · {}",
                e.friendly_name.as_deref().unwrap_or("Audio output")
            ),
            kind: AudioSourceKind::System,
            process_id: None,
            app_identifier: None,
            is_active: true,
            route: AudioRoute::Ignore,
            muted: false,
        })
        .collect())
}

pub(crate) fn excluded_source(id: &AudioSourceId) -> Option<AudioSourceId> {
    if id.0.len() > 65_600 {
        return None;
    }
    let encoded = id.0.strip_prefix(REST_PREFIX)?;
    let source = AudioSourceId(encoded.to_owned());
    crate::application_sessions::decode_application_session_id(&source.0)?;
    Some(source)
}

pub(crate) fn append_complements(sources: &mut Vec<AudioSource>) -> Result<(), RecorderError> {
    if crate::windows_build_number()? < 20348 {
        return Ok(());
    }
    let mut seen = std::collections::BTreeSet::new();
    let complements: Vec<_> = sources
        .iter()
        .filter(|s| s.kind == AudioSourceKind::Application && s.is_active)
        .filter_map(|app| {
            let identity = app.capture_identity.as_ref()?;
            let token = identity.process_instance.as_ref()?;
            if !seen.insert(token.clone()) {
                return None;
            }
            Some(AudioSource {
                controls: Default::default(),
                capture_identity: app.capture_identity.clone(),
                id: AudioSourceId(format!("{REST_PREFIX}{}", app.id.0)),
                kind: AudioSourceKind::System,
                display_name: format!("Other system audio · excludes {}", app.display_name),
                process_id: app.process_id,
                app_identifier: app.app_identifier.clone(),
                is_active: true,
                route: AudioRoute::Ignore,
                muted: false,
            })
        })
        .collect();
    sources.extend(complements);
    Ok(())
}
