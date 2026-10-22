//! Latest per-source levels from identity-checked, unmixed PCM. No sample ownership changes.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use frame_contracts::{AudioLevel, AudioLevelsSnapshot, AudioSourceId};

const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Accumulator {
    sum_squares: f64,
    count: u64,
    peak: f32,
    clipping: bool,
    last_publish: Option<Instant>,
}

struct Entry {
    level: AudioLevel,
    pending: Accumulator,
}

struct State {
    session_id: Option<String>,
    entries: Vec<Entry>,
}

/// A single replaceable snapshot. The mutex protects only small numeric state; PCM
/// blocks are never copied into it or retained after their pipeline admission.
pub struct MeterHub(Mutex<State>);

impl Default for MeterHub {
    fn default() -> Self {
        Self(Mutex::new(State {
            session_id: None,
            entries: Vec::new(),
        }))
    }
}

impl MeterHub {
    pub fn begin(&self, session_id: String, sources: &[AudioSourceId]) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.session_id = Some(session_id);
        state.entries = sources
            .iter()
            .map(|source_id| Entry {
                level: AudioLevel {
                    source_id: source_id.clone(),
                    rms: None,
                    peak: None,
                    clipping: None,
                },
                pending: Accumulator::default(),
            })
            .collect();
        state
            .entries
            .sort_unstable_by(|a, b| a.level.source_id.cmp(&b.level.source_id));
    }

    pub fn end(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.session_id = None;
        state.entries.clear();
    }

    pub fn snapshot(&self) -> AudioLevelsSnapshot {
        let state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        AudioLevelsSnapshot {
            session_id: state.session_id.clone(),
            sources: state
                .entries
                .iter()
                .map(|entry| entry.level.clone())
                .collect(),
        }
    }

    /// Accumulate every real sample, and publish at most once per 50 ms per source.
    /// Zero-valued samples are measured silence; no PCM leaves the value unmeasured.
    pub fn observe(&self, session_id: &str, source_id: &AudioSourceId, samples: &[f32]) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.session_id.as_deref() != Some(session_id) {
            return;
        }
        let Ok(index) = state
            .entries
            .binary_search_by(|entry| entry.level.source_id.cmp(source_id))
        else {
            return;
        };
        let entry = &mut state.entries[index];
        if samples.iter().any(|sample| !sample.is_finite()) {
            entry.level.rms = None;
            entry.level.peak = None;
            entry.level.clipping = None;
            entry.pending = Accumulator::default();
            return;
        }
        let now = Instant::now();
        let pending = &mut entry.pending;
        for &sample in samples {
            let absolute = sample.abs();
            pending.sum_squares += f64::from(sample) * f64::from(sample);
            pending.count += 1;
            pending.peak = pending.peak.max(absolute);
            pending.clipping |= absolute >= 1.0;
        }
        if pending.count > 0
            && pending
                .last_publish
                .is_none_or(|last| now.duration_since(last) >= PUBLISH_INTERVAL)
        {
            entry.level.rms = Some((pending.sum_squares / pending.count as f64).sqrt() as f32);
            entry.level.peak = Some(pending.peak);
            entry.level.clipping = Some(pending.clipping);
            *pending = Accumulator {
                last_publish: Some(now),
                ..Accumulator::default()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pcm_levels_and_identity() {
        let hub = MeterHub::default();
        let id = AudioSourceId("mic".into());
        hub.begin("one".into(), std::slice::from_ref(&id));
        assert_eq!(hub.snapshot().sources[0].rms, None);
        hub.observe("other", &id, &[1.0]);
        assert_eq!(hub.snapshot().sources[0].rms, None);
        hub.observe("one", &id, &[0.5, -0.5]);
        let level = &hub.snapshot().sources[0];
        assert_eq!(level.rms, Some(0.5));
        assert_eq!(level.peak, Some(0.5));
        assert_eq!(level.clipping, Some(false));
        hub.observe("one", &id, &[1.2]);
        // The first published value stays in place until the 50 ms window expires.
        assert_eq!(hub.snapshot().sources[0].clipping, Some(false));
        std::thread::sleep(PUBLISH_INTERVAL);
        hub.observe("one", &id, &[]);
        assert_eq!(hub.snapshot().sources[0].clipping, Some(true));
        assert_eq!(hub.snapshot().sources[0].peak, Some(1.2));
        hub.begin("silence".into(), std::slice::from_ref(&id));
        hub.observe("silence", &id, &[0.0, 0.0, 0.0]);
        let silent = &hub.snapshot().sources[0];
        assert_eq!(
            (silent.rms, silent.peak, silent.clipping),
            (Some(0.0), Some(0.0), Some(false))
        );
        hub.end();
        assert!(hub.snapshot().session_id.is_none());
    }
}
