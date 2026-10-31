//! Shared audio admission used by settings, native preparation and UI adapters.
use frame_contracts::*;

pub fn overlaps(a: &AudioSource, b: &AudioSource) -> bool {
    if a.id == b.id || (a.id.0.starts_with("wasapi-") && a.id.0.eq_ignore_ascii_case(&b.id.0)) {
        return true;
    }
    // A complement is disjoint only from its exact excluded tree and known children.
    // Two system captures always overlap; no sample subtraction or duplicate mixing.
    if a.kind == AudioSourceKind::System || b.kind == AudioSourceKind::System {
        let (system, other) = if a.kind == AudioSourceKind::System {
            (a, b)
        } else {
            (b, a)
        };
        if other.kind == AudioSourceKind::System {
            return true;
        }
        if other.kind == AudioSourceKind::Microphone {
            return false;
        }
        if system.id.0.starts_with("wasapi-rest:")
            && other.kind == AudioSourceKind::Application
            && let (Some(system), Some(app)) = (&system.capture_identity, &other.capture_identity)
            && let (Some(excluded), Some(target)) =
                (&system.process_instance, &app.process_instance)
        {
            return excluded != target && !app.ancestor_instances.contains(excluded);
        }
        return true;
    }
    native_overlap(a, b)
        || (a.kind == AudioSourceKind::Application
            && b.kind == AudioSourceKind::Application
            && !matches!((&a.capture_identity, &b.capture_identity), (Some(a), Some(b))
                if a.process_instance.is_some() && b.process_instance.is_some())
            && a.process_id
                .is_some_and(|pid| pid != 0 && Some(pid) == b.process_id)
            && ((a.id.0.starts_with("wasapi-session:") && b.id.0.starts_with("wasapi-session:"))
                || (a.id.0.starts_with("app:") && b.id.0.starts_with("app:"))))
}

fn native_overlap(a: &AudioSource, b: &AudioSource) -> bool {
    let (Some(a), Some(b)) = (&a.capture_identity, &b.capture_identity) else {
        return false;
    };
    if let (Some(a), Some(b)) = (&a.endpoint, &b.endpoint)
        && a.eq_ignore_ascii_case(b)
    {
        return true;
    }
    match (&a.process_instance, &b.process_instance) {
        (Some(ap), Some(bp)) => {
            ap == bp || a.ancestor_instances.contains(bp) || b.ancestor_instances.contains(ap)
        }
        _ => false,
    }
}

pub fn system_capture_supported(source: &AudioSource) -> bool {
    source.kind == AudioSourceKind::System
        && source.is_active
        && (source.id.0.starts_with("wasapi-loopback:") || source.id.0.starts_with("wasapi-rest:"))
}

pub fn validate_audio_selection(
    routes: &[AudioRouteSelection],
    sources: &[AudioSource],
    per_app: bool,
    microphone: bool,
) -> Result<(), RecorderError> {
    validate_audio_topology(routes)?;
    for route in routes {
        route.controls.validate()?;
    }
    let enabled: Vec<_> = routes
        .iter()
        .filter(|r| r.route != AudioRoute::Ignore)
        .collect();
    let mut selected = Vec::new();
    for route in enabled {
        let source = sources
            .iter()
            .find(|s| s.id == route.source_id && s.is_active)
            .ok_or_else(|| {
                RecorderError::invalid(format!(
                    "Audio source {} is unavailable. Refresh devices or turn it off.",
                    route.source_id.0
                ))
            })?;
        if (source.kind == AudioSourceKind::System
            && (!system_capture_supported(source)
                || (source.id.0.starts_with("wasapi-rest:") && !per_app)))
            || (source.kind == AudioSourceKind::Application && !per_app)
            || (source.kind == AudioSourceKind::Microphone && !microphone)
        {
            return Err(RecorderError::unsupported("Selected audio capture kind"));
        }
        if selected.iter().any(|other| overlaps(source, other)) {
            return Err(RecorderError::invalid(
                "Selected sources capture overlapping audio. Keep one source enabled.",
            ));
        }
        selected.push(source.clone());
    }
    Ok(())
}

pub fn validate_audio_topology(routes: &[AudioRouteSelection]) -> Result<(), RecorderError> {
    for route in routes {
        route.controls.validate()?;
    }
    let mut ids = std::collections::BTreeSet::new();
    if routes
        .iter()
        .any(|r| r.source_id.0.trim().is_empty() || !ids.insert(&r.source_id))
    {
        return Err(RecorderError::invalid("Duplicate audio capture source."));
    }
    Ok(())
}
