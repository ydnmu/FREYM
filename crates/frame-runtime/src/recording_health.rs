//! Read-only projections of actual recorder observations. Unknown measurements
//! remain unknown; configured rates, clock residuals and GPU load are distinct.
use frame_contracts::{RecorderMetrics, RecorderState};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticRow {
    pub label: &'static str,
    pub value: String,
}

fn observed(value: Option<f64>, unit: &str) -> String {
    value
        .filter(|v| v.is_finite())
        .map(|v| format!("{v:.2}{unit}"))
        .unwrap_or_else(|| "Unmeasured".into())
}

pub fn diagnostics(metrics: Option<&RecorderMetrics>) -> Vec<DiagnosticRow> {
    let Some(m) = metrics else {
        return vec![DiagnosticRow {
            label: "Recording observations",
            value: "No session measurements yet".into(),
        }];
    };
    let rate = &m.frame_rate;
    let mut rows = vec![
        DiagnosticRow {
            label: "Active recording time",
            value: observed(Some(m.elapsed_ms / 1000.0), " s"),
        },
        DiagnosticRow {
            label: "Requested rate (configuration)",
            value: observed(rate.requested_fps, " FPS"),
        },
        DiagnosticRow {
            label: "Source nominal rate (configuration)",
            value: observed(rate.source_rate_hz, " Hz"),
        },
        DiagnosticRow {
            label: "Observed source presentations",
            value: observed(rate.observed_source_fps, " FPS"),
        },
        DiagnosticRow {
            label: "Capture delivered",
            value: observed(rate.actual_capture_fps, " FPS"),
        },
        DiagnosticRow {
            label: "Encoder admitted",
            value: observed(rate.submitted_fps, " FPS"),
        },
        DiagnosticRow {
            label: "Encoded pictures",
            value: observed(rate.encoded_fps, " FPS"),
        },
        DiagnosticRow {
            label: "Capture latency",
            value: observed(m.capture_latency_ms, " ms"),
        },
        DiagnosticRow {
            label: "Encoding latency",
            value: observed(m.encode_latency_ms, " ms"),
        },
        DiagnosticRow {
            label: "Content A/V offset",
            value: observed(m.av_offset_ms, " ms"),
        },
        DiagnosticRow {
            label: "Hardware encoder utilization",
            value: "Unmeasured".into(),
        },
        DiagnosticRow {
            label: "Duplicate encoded frames",
            value: "Unmeasured; source presentation identities do not establish pixel duplicates"
                .into(),
        },
    ];
    // The engine's total contains three disjoint capture loss categories. Keep
    // policy skips, backpressure returns and DXGI upstream estimates separate.
    if let Some(c) = &rate.counters {
        rows.extend([
            DiagnosticRow {
                label: "Observed native source presentation count",
                value: c.source_presentations.to_string(),
            },
            DiagnosticRow {
                label: "Frames: captured / source identities / submitted / encoded",
                value: format!(
                    "{} / {} / {} / {}",
                    c.captured_frames,
                    c.captured_unique_frames,
                    c.submitted_frames,
                    c.encoded_frames
                ),
            },
            DiagnosticRow {
                label: "Capture pool pressure drops",
                value: c.pool_pressure_drops.to_string(),
            },
            DiagnosticRow {
                label: "Capture ready queue drops",
                value: c.ready_queue_drops.to_string(),
            },
            DiagnosticRow {
                label: "Capture contention drops",
                value: c.contention_drops.to_string(),
            },
            DiagnosticRow {
                label: "Intentional FPS policy skips",
                value: c.policy_skipped_frames.to_string(),
            },
            DiagnosticRow {
                label: "Encoder backpressure events",
                value: c.backpressure_events.to_string(),
            },
            DiagnosticRow {
                label: "DXGI upstream update estimate",
                value: c.upstream_accumulated_updates.to_string(),
            },
        ]);
    }
    rows.push(DiagnosticRow {
        label: "Measured capture loss total",
        value: rate
            .dropped_frames
            .map(|n| n.to_string())
            .unwrap_or_else(|| "Unmeasured".into()),
    });
    for q in &m.queues {
        rows.push(DiagnosticRow {
            label: "Queue pressure",
            value: format!(
                "{}: {} / {} · queue-local drops {}{}",
                q.name,
                q.depth,
                q.capacity,
                q.dropped,
                if q.capacity > 0 && q.depth >= q.capacity {
                    " · full at this observation"
                } else {
                    ""
                }
            ),
        });
    }
    rows
}

/// Integrity verification of a completed indexed recording. This is a blocking,
/// read-only file operation: run off the UI and recording owner. CRC and manifest
/// checks are not decoder validation or a content synchronization test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingVerification {
    pub segments: usize,
    pub bytes: u64,
    pub encoder_ids: Vec<String>,
}

pub fn verification_allowed(state: &RecorderState, replay_active: bool) -> bool {
    matches!(state, RecorderState::Idle) && !replay_active
}

pub fn verify_finalized(path: &Path) -> Result<RecordingVerification, String> {
    if !path.is_absolute()
        || path
            .extension()
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("mkv"))
    {
        return Err("Choose an absolute finalized MKV path.".into());
    }
    let mut index_name = path.as_os_str().to_os_string();
    index_name.push(".freym-session");
    let index = std::path::PathBuf::from(index_name);
    if !index
        .try_exists()
        .map_err(|e| format!("Cannot check the recording session: {e}"))?
    {
        return Err("This recording has no indexed session manifest. Legacy/unindexed recordings require a decoder/playback check.".into());
    }
    let manifest = frame_mux::read_session_manifest(&index).map_err(|e| e.message)?;
    if !manifest.completed || manifest.segments.is_empty() {
        return Err("The session did not complete durable finalization. Use recovery on an interrupted recording.".into());
    }
    let folder = index
        .parent()
        .ok_or("The session has no parent directory.")?;
    let mut bytes = 0_u64;
    let mut codecs = Vec::new();
    for segment in &manifest.segments {
        frame_mux::verify_session_segment(&folder.join(&segment.file_name), segment)
            .map_err(|e| e.message)?;
        bytes = bytes
            .checked_add(segment.file_bytes)
            .ok_or("Session byte count overflowed.")?;
        if !codecs.contains(&segment.encoder_id) {
            codecs.push(segment.encoder_id.clone());
        }
    }
    Ok(RecordingVerification {
        segments: manifest.segments.len(),
        bytes,
        encoder_ids: codecs,
    })
}
