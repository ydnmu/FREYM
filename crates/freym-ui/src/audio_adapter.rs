//! Audio-only admission and routing. No capture, DSP or UI timing lives here.
#[cfg(test)]
#[path = "audio_adapter_tests.rs"]
mod tests;
use frame_audio::{TrackPlan, plan_tracks};
use frame_contracts::*;

pub fn selections(sources: &[AudioSource]) -> Vec<AudioRouteSelection> {
    sources
        .iter()
        .map(|s| AudioRouteSelection {
            controls: s.controls,
            source_id: s.id.clone(),
            route: s.route,
            muted: s.muted,
        })
        .collect()
}

#[allow(dead_code)]
pub fn main(route: AudioRoute) -> bool {
    matches!(route, AudioRoute::MainMix | AudioRoute::MainMixAndSeparate)
}
pub fn individual(route: AudioRoute) -> bool {
    matches!(
        route,
        AudioRoute::SeparateOnly | AudioRoute::MainMixAndSeparate
    )
}
pub fn route(main: bool, individual: bool) -> AudioRoute {
    match (main, individual) {
        (false, false) => AudioRoute::Ignore,
        (true, false) => AudioRoute::MainMix,
        (false, true) => AudioRoute::SeparateOnly,
        (true, true) => AudioRoute::MainMixAndSeparate,
    }
}

/// Session IDs can differ even when Windows captures the same process tree.
/// Linux's Pulse stream IDs represent distinct streams, even with the same PID.
pub fn overlap(a: &AudioSource, b: &AudioSource) -> bool {
    frame_settings::audio_admission::overlaps(a, b)
}

pub fn conflicts(sources: &[AudioSource]) -> Vec<String> {
    let enabled: Vec<_> = sources
        .iter()
        .filter(|s| s.route != AudioRoute::Ignore)
        .collect();
    let mut result = Vec::new();
    for (index, a) in enabled.iter().enumerate() {
        for b in &enabled[index + 1..] {
            if overlap(a, b) {
                result.push(format!(
                    "{} and {} capture overlapping audio. Keep one source enabled.",
                    a.display_name, b.display_name
                ));
            }
        }
    }
    result
}

pub fn validate(sources: &[AudioSource], per_app: bool) -> Result<Vec<TrackPlan>, RecorderError> {
    frame_settings::audio_admission::validate_audio_selection(
        &selections(sources),
        sources,
        per_app,
        true,
    )?;
    let plan = plan_tracks(&selections(sources))?;
    if let Some(message) = conflicts(sources).first() {
        return Err(RecorderError::invalid(message.clone()));
    }
    let active: Vec<_> = sources
        .iter()
        .filter(|s| s.route != AudioRoute::Ignore)
        .collect();
    if let Some(source) = active.iter().find(|s| !s.is_active) {
        return Err(RecorderError::invalid(format!(
            "{} is unavailable. Reconnect it, refresh devices, or turn it off.",
            source.display_name
        )));
    }
    if !per_app
        && active
            .iter()
            .any(|s| s.kind == AudioSourceKind::Application)
    {
        return Err(RecorderError::unsupported("Application audio capture"));
    }
    Ok(plan)
}

/// Validate the final batch, then send only real changes through UpdateRoute.
/// Turning sources off must remain possible for an already-invalid saved setup.
pub fn edits(
    sources: &[AudioSource],
    changes: &[(AudioSourceId, AudioRoute)],
    per_app: bool,
) -> Result<Vec<(AudioSourceId, AudioRoute)>, RecorderError> {
    let mut candidate = sources.to_vec();
    let mut seen = std::collections::BTreeSet::new();
    for (id, next) in changes {
        if !seen.insert(id.clone()) {
            return Err(RecorderError::invalid("Duplicate audio routing edit."));
        }
        let source = candidate.iter_mut().find(|s| &s.id == id).ok_or_else(|| {
            RecorderError::invalid("This audio source is no longer available. Refresh devices.")
        })?;
        source.route = *next;
    }
    let only_removes = changes
        .iter()
        .all(|(_, route)| *route == AudioRoute::Ignore);
    if !only_removes {
        validate(&candidate, per_app)?;
    }
    let mut edits: Vec<_> = changes
        .iter()
        .filter(|(id, route)| sources.iter().any(|s| &s.id == id && s.route != *route))
        .cloned()
        .collect();
    // Disable old captures before enabling replacements, even when a caller
    // supplies the final batch in the opposite order.
    edits.sort_by_key(|(_, route)| *route != AudioRoute::Ignore);
    Ok(edits)
}

pub fn separate_edits(sources: &[AudioSource], enabled: bool) -> Vec<(AudioSourceId, AudioRoute)> {
    sources
        .iter()
        .filter(|s| s.route != AudioRoute::Ignore)
        .map(|s| (s.id.clone(), route(true, enabled)))
        .collect()
}
