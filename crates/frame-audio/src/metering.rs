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
    /// Presentation read: never waits on the PCM observer, and never returns stale
    /// levels as a live measurement. The wire snapshot contract remains unchanged.
    pub fn try_snapshot_fresh(&self, max_age: Duration) -> Option<AudioLevelsSnapshot> {
        let state = match self.0.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let now = Instant::now();
        Some(AudioLevelsSnapshot {
            session_id: state.session_id.clone(),
            sources: state
                .entries
                .iter()
                .map(|entry| {
                    let mut level = entry.level.clone();
                    if entry
                        .pending
                        .last_publish
                        .is_none_or(|at| now.saturating_duration_since(at) > max_age)
                    {
                        level.rms = None;
                        level.peak = None;
                        level.clipping = None;
                    }
                    level
                })
                .collect(),
        })
    }

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
    fn presentation_snapshot_is_nonblocking_and_rejects_stale_or_invalid_pcm() {
        let hub = MeterHub::default();
        let id = AudioSourceId("source".into());
        let age = Duration::from_millis(250);
        hub.begin("live".into(), std::slice::from_ref(&id));
        hub.observe("live", &id, &[0.5, -0.5, 0., 0.]);
        let snapshot = hub.try_snapshot_fresh(age).unwrap();
        assert_eq!(snapshot.sources[0].peak, Some(0.5));
        assert!((snapshot.sources[0].rms.unwrap() - (0.125f32).sqrt()).abs() < 0.000001);
        {
            let mut guard = hub.0.lock().unwrap();
            assert!(
                hub.try_snapshot_fresh(age).is_none(),
                "The display must not wait on the PCM observer"
            );
            guard.entries[0].pending.last_publish =
                Some(Instant::now() - age - Duration::from_millis(1));
        }
        let stale = hub.try_snapshot_fresh(age).unwrap();
        assert_eq!(
            (
                stale.sources[0].rms,
                stale.sources[0].peak,
                stale.sources[0].clipping
            ),
            (None, None, None)
        );
        hub.observe("live", &id, &[0., 0.]);
        assert_eq!(
            hub.try_snapshot_fresh(age).unwrap().sources[0].rms,
            Some(0.)
        );
        hub.observe("live", &id, &[f32::NAN]);
        assert!(
            hub.try_snapshot_fresh(age).unwrap().sources[0]
                .peak
                .is_none()
        );
        hub.end();
        assert!(hub.try_snapshot_fresh(age).unwrap().session_id.is_none());
    }
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
