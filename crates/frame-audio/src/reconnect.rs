// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Bounded source reopen without changing frozen track identities or media epochs.

use crate::{
    AudioCancellation, AudioContext, AudioContinuityPolicy, AudioReopenFactory, AudioSession,
    DrainableAudioSession, DriftCompensationHealth, NormalizeCounters, NormalizingAudioSession,
    PcmBlock, PcmDrainPoll, SourceFormat,
};
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{AudioSourceId, ErrorCode, RecorderError};
use std::sync::Arc;

const RETRY_INTERVAL_NS: u64 = 250_000_000;
const RECONNECT_LIMIT_NS: u64 = 5_000_000_000;
const MAX_REOPEN_ATTEMPTS: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceLifecycle {
    Connected { generation: u64 },
    Reconnecting { generation: u64, attempts: u32 },
    Ended { generation: u64 },
}

impl SourceLifecycle {
    pub const fn generation(self) -> u64 {
        match self {
            Self::Connected { generation }
            | Self::Reconnecting { generation, .. }
            | Self::Ended { generation } => generation,
        }
    }
}

struct Reconnect {
    started: MediaTimestamp,
    next_attempt: MediaTimestamp,
    attempts: u32,
    failure: RecorderError,
}

/// A missing source produces explicit mixer gaps while its exact stable ID is
/// retried. Native timestamps come from the original recording clock. Each reopened
/// generation gets fresh format/drift/resampler state, never a synthetic time shift.
pub struct RecoveringAudioSession<S: AudioSession> {
    inner: Option<NormalizingAudioSession<S>>,
    source_id: AudioSourceId,
    clock: Arc<MasterClock>,
    owner_cancel: AudioCancellation,
    policy: AudioContinuityPolicy,
    reopen: Option<AudioReopenFactory<S>>,
    reconnect: Option<Reconnect>,
    generation: u64,
    draining: bool,
    stopped: bool,
    failure: Option<RecorderError>,
    cleanup_failure: Option<RecorderError>,
    preserve_channels: bool,
    tolerate_source_loss: bool,
    channels: std::num::NonZeroU16,
}

impl<S: AudioSession> RecoveringAudioSession<S> {
    pub(crate) fn new(
        inner: S,
        source_id: AudioSourceId,
        clock: Arc<MasterClock>,
        owner_cancel: AudioCancellation,
        policy: AudioContinuityPolicy,
        reopen: Option<AudioReopenFactory<S>>,
    ) -> Self {
        Self {
            inner: Some(
                NormalizingAudioSession::new(inner, source_id.clone())
                    .with_drift_compensation(policy.compensate_drift),
            ),
            source_id,
            clock,
            owner_cancel,
            policy,
            reopen,
            reconnect: None,
            generation: 0,
            draining: false,
            stopped: false,
            failure: None,
            cleanup_failure: None,
            preserve_channels: false,
            tolerate_source_loss: false,
            channels: std::num::NonZeroU16::new(2).unwrap(),
        }
    }

    pub fn for_recording(mut self) -> Self {
        if let Some(inner) = self.inner.take() {
            let inner = inner.preserving_channels();
            self.channels = inner.pcm_channels();
            self.inner = Some(inner);
        }
        self.preserve_channels = true;
        self.tolerate_source_loss = true;
        self
    }

    pub fn source_format(&self) -> Option<SourceFormat> {
        self.inner
            .as_ref()
            .and_then(NormalizingAudioSession::source_format)
    }

    pub fn is_resampling(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(NormalizingAudioSession::is_resampling)
    }

    /// Counters of the current native generation, never a fabricated cumulative total.
    pub fn counters(&self) -> NormalizeCounters {
        self.inner.as_ref().map_or(
            NormalizeCounters::default(),
            NormalizingAudioSession::counters,
        )
    }

    fn source_error(&mut self, error: RecorderError) -> Result<Option<PcmBlock>, RecorderError> {
        if error.code != ErrorCode::SourceLost
            || !self.policy.reconnect_source
            || self.reopen.is_none()
            || self.draining
            || self.owner_cancel.is_cancelled()
        {
            if self.tolerate_source_loss && error.code == ErrorCode::SourceLost {
                if let Some(mut old) = self.inner.take() {
                    old.stop_for_continuity()?;
                }
                self.reconnect = None;
                return Ok(None);
            }
            self.failure = Some(error.clone());
            return Err(error);
        }
        if let Some(mut old) = self.inner.take() {
            // SourceLost is the already-reported worker failure; stop still joins it.
            // A different cleanup failure makes reopen unsafe and stays terminal.
            if let Err(cleanup) = old.stop_for_continuity() {
                self.cleanup_failure = Some(cleanup.clone());
                let error = RecorderError {
                    message: format!("{} Cleanup failed: {}", error.message, cleanup.message),
                    ..error
                };
                self.failure = Some(error.clone());
                return Err(error);
            }
        }
        let now = self.clock.now();
        self.reconnect = Some(Reconnect {
            started: now,
            next_attempt: now,
            attempts: 0,
            failure: error,
        });
        Ok(None)
    }

    fn service_reconnect(&mut self, now: MediaTimestamp) -> Result<(), RecorderError> {
        let Some(state) = self.reconnect.as_mut() else {
            return Ok(());
        };
        if now.0.saturating_sub(state.started.0) >= RECONNECT_LIMIT_NS
            || state.attempts >= MAX_REOPEN_ATTEMPTS
        {
            let mut error = state.failure.clone();
            error.message = format!(
                "{} Same-source reopen exhausted after {} attempts.",
                error.message, state.attempts
            );
            if self.tolerate_source_loss {
                self.reconnect = None;
                return Ok(());
            }
            self.failure = Some(error.clone());
            return Err(error);
        }
        if now < state.next_attempt {
            return Ok(());
        }
        state.attempts += 1;
        state.next_attempt = MediaTimestamp(now.0.saturating_add(RETRY_INTERVAL_NS));
        let context = AudioContext {
            clock: Arc::clone(&self.clock),
            cancellation: self.owner_cancel.child(),
        };
        match self.reopen.expect("reconnect requires factory")(&self.source_id, context) {
            Ok(mut session) => {
                if self.preserve_channels && session.pcm_channels() != self.channels {
                    session.stop()?;
                    self.reconnect = None;
                    return Ok(());
                }
                self.inner = Some(
                    NormalizingAudioSession::new(session, self.source_id.clone())
                        .with_drift_compensation(self.policy.compensate_drift),
                );
                if self.preserve_channels {
                    self.inner = self
                        .inner
                        .take()
                        .map(NormalizingAudioSession::preserving_channels);
                }
                self.generation = self.generation.saturating_add(1);
                self.reconnect = None;
            }
            Err(error) if error.code == ErrorCode::SourceLost => state.failure = error,
            Err(error) => {
                self.failure = Some(error.clone());
                return Err(error);
            }
        }
        Ok(())
    }
}

impl<S: AudioSession> AudioSession for RecoveringAudioSession<S> {
    fn allows_silent_gaps(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(AudioSession::allows_silent_gaps)
    }
    fn pcm_channels(&self) -> std::num::NonZeroU16 {
        self.channels
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.stopped {
            return Ok(None);
        }
        if self.owner_cancel.is_cancelled() {
            self.stop()?;
            return Ok(None);
        }
        if self.reconnect.is_some() {
            self.service_reconnect(self.clock.now())?;
        }
        let Some(inner) = &mut self.inner else {
            return Ok(None);
        };
        match inner.try_pcm() {
            Err(error) => self.source_error(error),
            result => result,
        }
    }

    fn stop(&mut self) -> Result<(), RecorderError> {
        if self.stopped {
            return self.failure.clone().map_or(Ok(()), Err);
        }
        self.stopped = true;
        self.reconnect = None;
        if let Some(mut inner) = self.inner.take() {
            if let Err(error) = inner.stop()
                && self.failure.is_none()
            {
                self.failure = Some(error);
            }
            if let Err(error) = inner.stop_for_continuity() {
                self.cleanup_failure = Some(error);
            }
        }
        self.failure.clone().map_or(Ok(()), Err)
    }

    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        let result = self.stop();
        if let Some(cleanup) = &self.cleanup_failure {
            return Err(cleanup.clone());
        }
        match result {
            Err(error) if error.code != ErrorCode::SourceLost => Err(error),
            _ => Ok(()),
        }
    }

    fn clock_drift(&self) -> Option<frame_clock::DriftEstimate> {
        self.inner.as_ref().and_then(AudioSession::clock_drift)
    }

    fn compensation_health(&self) -> Option<DriftCompensationHealth> {
        self.inner
            .as_ref()
            .and_then(AudioSession::compensation_health)
    }

    fn lifecycle(&self) -> SourceLifecycle {
        if self.stopped
            || self.failure.is_some()
            || (self.inner.is_none() && self.reconnect.is_none())
        {
            SourceLifecycle::Ended {
                generation: self.generation,
            }
        } else if let Some(reconnect) = &self.reconnect {
            SourceLifecycle::Reconnecting {
                generation: self.generation,
                attempts: reconnect.attempts,
            }
        } else {
            SourceLifecycle::Connected {
                generation: self.generation,
            }
        }
    }
}

impl<S: DrainableAudioSession> DrainableAudioSession for RecoveringAudioSession<S> {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.stopped {
            return Err(RecorderError::invalid("The source is already stopped."));
        }
        if self.draining {
            return Ok(());
        }
        self.draining = true;
        // No unpublished source audio can be reconstructed while disconnected.
        self.reconnect = None;
        if let Some(inner) = &mut self.inner {
            inner.request_drain()?;
        }
        Ok(())
    }

    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if !self.draining || self.stopped {
            return Err(RecorderError::invalid("The source drain is not active."));
        }
        match self
            .inner
            .as_mut()
            .map_or(Ok(PcmDrainPoll::Drained), DrainableAudioSession::poll_drain)
        {
            Err(error) => {
                self.source_error(error)?;
                Ok(PcmDrainPoll::Drained)
            }
            result => result,
        }
    }
}

impl<S: AudioSession> Drop for RecoveringAudioSession<S> {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Device {
        lost: bool,
        cleanup_fails: bool,
        cancellation: AudioCancellation,
    }
    std::thread_local! { static REOPEN_CLOCK: std::cell::RefCell<Option<Arc<MasterClock>>> = const { std::cell::RefCell::new(None) }; }
    fn lost() -> RecorderError {
        RecorderError {
            code: ErrorCode::SourceLost,
            ..RecorderError::invalid("device unplugged")
        }
    }
    impl AudioSession for Device {
        fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
            if self.lost { Err(lost()) } else { Ok(None) }
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            self.cancellation.cancel();
            if self.cleanup_fails {
                return Err(RecorderError {
                    message: "device unplugged; cleanup failed".into(),
                    ..lost()
                });
            }
            if self.lost { Err(lost()) } else { Ok(()) }
        }
        fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
            let result = self.stop();
            if self.lost && !self.cleanup_fails {
                Ok(())
            } else {
                result
            }
        }
    }
    impl DrainableAudioSession for Device {
        fn request_drain(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
        fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
            Ok(PcmDrainPoll::Drained)
        }
    }
    fn reopen(id: &AudioSourceId, context: AudioContext) -> Result<Device, RecorderError> {
        assert_eq!(id.0, "exact-device");
        assert!(!context.cancellation.is_cancelled());
        REOPEN_CLOCK.with(|clock| *clock.borrow_mut() = Some(context.clock));
        Ok(Device {
            lost: false,
            cleanup_fails: false,
            cancellation: context.cancellation,
        })
    }
    fn session(factory: crate::AudioReopenFactory<Device>) -> RecoveringAudioSession<Device> {
        session_with_cleanup(factory, false)
    }
    fn session_with_cleanup(
        factory: crate::AudioReopenFactory<Device>,
        cleanup_fails: bool,
    ) -> RecoveringAudioSession<Device> {
        let clock = Arc::new(MasterClock::default());
        let cancellation = AudioCancellation::default();
        RecoveringAudioSession::new(
            Device {
                lost: true,
                cleanup_fails,
                cancellation: cancellation.child(),
            },
            AudioSourceId("exact-device".into()),
            clock,
            cancellation,
            AudioContinuityPolicy {
                compensate_drift: false,
                reconnect_source: true,
            },
            Some(factory),
        )
    }
    #[test]
    fn a_cleanup_failure_with_the_primary_code_never_reopens() {
        fn forbidden(_: &AudioSourceId, _: AudioContext) -> Result<Device, RecorderError> {
            panic!("unsafe cleanup must prevent a new native source");
        }
        let mut source = session_with_cleanup(forbidden, true);
        let error = source.try_pcm().err().unwrap();
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(error.message.contains("Cleanup failed"));
        assert_eq!(source.try_pcm().err().unwrap(), error);
        assert_eq!(source.lifecycle(), SourceLifecycle::Ended { generation: 0 });
        assert!(source.stop_for_continuity().is_err());
    }
    #[test]
    fn same_source_reopens_with_original_clock_and_drain_never_reconnects() {
        let mut source = session(reopen);
        assert!(source.try_pcm().unwrap().is_none());
        assert!(!source.owner_cancel.is_cancelled());
        assert!(matches!(
            source.lifecycle(),
            SourceLifecycle::Reconnecting { generation: 0, .. }
        ));
        assert!(source.try_pcm().unwrap().is_none());
        assert_eq!(
            source.lifecycle(),
            SourceLifecycle::Connected { generation: 1 }
        );
        REOPEN_CLOCK
            .with(|clock| assert!(Arc::ptr_eq(clock.borrow().as_ref().unwrap(), &source.clock)));
        source.request_drain().unwrap();
        assert!(matches!(
            source.poll_drain().unwrap(),
            PcmDrainPoll::Drained
        ));
        source.stop().unwrap();
        assert_eq!(source.lifecycle(), SourceLifecycle::Ended { generation: 1 });
    }
    #[test]
    fn outage_exhaustion_is_sticky_and_disconnected_drain_finishes() {
        fn missing(_: &AudioSourceId, _: AudioContext) -> Result<Device, RecorderError> {
            Err(lost())
        }
        let mut source = session(missing);
        source.try_pcm().unwrap();
        let began = source.reconnect.as_ref().unwrap().started;
        assert_eq!(
            source
                .service_reconnect(MediaTimestamp(began.0 + RECONNECT_LIMIT_NS))
                .unwrap_err()
                .code,
            ErrorCode::SourceLost
        );
        assert!(matches!(
            source.try_pcm(),
            Err(RecorderError {
                code: ErrorCode::SourceLost,
                ..
            })
        ));
        let mut source = session(missing);
        source.try_pcm().unwrap();
        source.request_drain().unwrap();
        assert!(matches!(
            source.poll_drain().unwrap(),
            PcmDrainPoll::Drained
        ));
        assert!(source.reconnect.is_none());
    }
}
