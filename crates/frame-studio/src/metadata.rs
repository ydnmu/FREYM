//! Adapter from the recording sidecar's exact session/segment origin to edit time.
use crate::*;
use frame_metadata::{EventKind, Record};
use std::path::Path;

pub fn read_recording_metadata(
    path: &Path,
    segment_origin_ns: u64,
    width: u32,
    height: u32,
    cursor_baked: bool,
) -> StudioResult<InteractionMetadata> {
    let mut result = InteractionMetadata {
        version: 1,
        width,
        height,
        cursor_baked,
        origin_ns: segment_origin_ns,
        sampling_interval_ns: 0,
        truncated: false,
        events: Vec::new(),
    };
    let mut geometry = None;
    let mut position = Point { x: 0.5, y: 0.5 };
    let mut carry = None;
    let mut carried = false;
    let summary = frame_metadata::read_sidecar(path, |record| {
        if let Record::Header(header) = record {
            result.cursor_baked = header.cursor_baked.unwrap_or(cursor_baked);
        }
        let Record::Event(event) = record else {
            return Ok(());
        };
        if let EventKind::Geometry { bounds } = event.event {
            geometry = Some(bounds);
        }
        let time_ns = event.session_ns.checked_sub(segment_origin_ns);
        let interaction = match event.event {
            EventKind::Cursor { x, y }
            | EventKind::Button {
                x,
                y,
                pressed: true,
                ..
            }
            | EventKind::Scroll { x, y, .. } => {
                let bounds = geometry.ok_or_else(|| {
                    std::io::Error::other("Cursor metadata precedes source geometry.")
                })?;
                if x >= bounds.width || y >= bounds.height {
                    return Err(std::io::Error::other(
                        "Cursor metadata is outside source geometry.",
                    ));
                }
                position = Point {
                    x: x as f64 / bounds.width as f64,
                    y: y as f64 / bounds.height as f64,
                };
                Some(match event.event {
                    EventKind::Button { .. } => InteractionKind::Click,
                    EventKind::Scroll { .. } => InteractionKind::Scroll,
                    _ => InteractionKind::Move,
                })
            }
            EventKind::Focus { active: true } => Some(InteractionKind::Focus),
            EventKind::CursorLeft
            | EventKind::SourceUnavailable
            | EventKind::Focus { active: false } => Some(InteractionKind::Leave),
            EventKind::Loss { dropped } => {
                result.truncated |= dropped > 0;
                Some(InteractionKind::Leave)
            }
            _ => None,
        };
        let Some(time_ns) = time_ns else {
            if let Some(kind) = interaction {
                carry = if kind == InteractionKind::Leave {
                    None
                } else {
                    Some(position)
                };
            }
            return Ok(());
        };
        // A stationary pointer can have no new sample after an encoder restart.
        // Carry its last visible position, without replaying a preceding click.
        if !carried {
            if let Some(position) = carry.take() {
                result.events.push(Interaction {
                    time_ns: 0,
                    position,
                    kind: InteractionKind::Move,
                });
            }
            carried = true;
        }
        if let Some(kind) = interaction {
            if result.events.len() >= MAX_EVENTS {
                return Err(std::io::Error::other("Studio event budget exceeded."));
            }
            result.events.push(Interaction {
                time_ns,
                position,
                kind,
            });
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    if !carried && let Some(position) = carry {
        result.events.push(Interaction {
            time_ns: 0,
            position,
            kind: InteractionKind::Move,
        });
    }
    result.truncated |= !summary.clean_end || !summary.completed || summary.dropped > 0;
    result.validate()?;
    Ok(result)
}
