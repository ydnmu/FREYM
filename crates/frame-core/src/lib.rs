//! Authoritative lifecycle. A command acknowledgement is not media readiness.
pub use frame_contracts::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(u64);
impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
pub struct Recorder {
    state: RecorderState,
    generation: u64,
}
impl Default for Recorder {
    fn default() -> Self {
        Self {
            state: RecorderState::Idle,
            generation: 0,
        }
    }
}
impl Recorder {
    pub fn state(&self) -> &RecorderState {
        &self.state
    }
    fn transition_error(&self) -> RecorderError {
        RecorderError {
            code: ErrorCode::InvalidTransition,
            message: format!("Command is unavailable while {:?}.", self.state),
            recovery: RecoveryStatus::Unknown,
            action: "Wait for the current operation to finish.".into(),
        }
    }
    fn check_session(&self, session: SessionId) -> Result<(), RecorderError> {
        if session.0 != self.generation || matches!(self.state, RecorderState::Idle) {
            return Err(self.transition_error());
        }
        Ok(())
    }
    /// Caller must validate native target/capabilities before preparing resources.
    pub fn begin(&mut self, request: &RecordingRequest) -> Result<SessionId, RecorderError> {
        if self.state != RecorderState::Idle {
            return Err(self.transition_error());
        }
        frame_settings::validate_request(request)?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| RecorderError::invalid("Session counter exhausted."))?;
        self.state = RecorderState::Preparing;
        Ok(SessionId(self.generation))
    }
    pub fn ready(&mut self, session: SessionId) -> Result<(), RecorderError> {
        self.check_session(session)?;
        if self.state != RecorderState::Preparing {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Recording;
        Ok(())
    }
    pub fn pause(&mut self) -> Result<(), RecorderError> {
        if self.state != RecorderState::Recording {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Paused;
        Ok(())
    }
    pub fn resume(&mut self) -> Result<(), RecorderError> {
        if self.state != RecorderState::Paused {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Recording;
        Ok(())
    }
    pub fn stop(&mut self) -> Result<(), RecorderError> {
        if !matches!(
            self.state,
            RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused
        ) {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Finalizing;
        Ok(())
    }
    /// Call only after cleanup and container finalization have actually completed.
    pub fn finalized(&mut self, session: SessionId) -> Result<(), RecorderError> {
        self.check_session(session)?;
        if self.state != RecorderState::Finalizing {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Idle;
        Ok(())
    }
    pub fn fail(&mut self, session: SessionId, error: RecorderError) -> Result<(), RecorderError> {
        self.check_session(session)?;
        if matches!(self.state, RecorderState::Failed(_)) {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Failed(error);
        Ok(())
    }
    /// Only the resource owner may reset after all failed-session resources are released.
    pub fn reset_after_cleanup(&mut self) -> Result<(), RecorderError> {
        if !matches!(self.state, RecorderState::Failed(_)) {
            return Err(self.transition_error());
        }
        self.state = RecorderState::Idle;
        Ok(())
    }
}

pub fn foundation_snapshot() -> EngineSnapshot {
    EngineSnapshot::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> RecordingRequest {
        RecordingRequest {
            target: CaptureTarget::Display(DisplayId("display".into())),
            fps_mode: FpsMode::AutoMatchSource,
            resolution: Resolution::Native,
            quality: Quality::High,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_routes: vec![],
            camera: None,
            show_cursor: true,
            output: OutputSettings {
                directory: "recordings".into(),
                file_stem: "test".into(),
            },
        }
    }
    #[test]
    fn complete_lifecycle_waits_for_native_acknowledgements() {
        let mut r = Recorder::default();
        let s = r.begin(&request()).unwrap();
        assert_eq!(r.state(), &RecorderState::Preparing);
        assert!(r.begin(&request()).is_err());
        r.ready(s).unwrap();
        r.pause().unwrap();
        r.resume().unwrap();
        r.stop().unwrap();
        assert_eq!(r.state(), &RecorderState::Finalizing);
        assert!(r.stop().is_err());
        r.finalized(s).unwrap();
        assert_eq!(r.state(), &RecorderState::Idle);
    }
    #[test]
    fn cancellation_and_stale_callbacks_are_rejected() {
        let mut r = Recorder::default();
        let old = r.begin(&request()).unwrap();
        r.stop().unwrap();
        assert!(r.ready(old).is_err());
        r.finalized(old).unwrap();
        let new = r.begin(&request()).unwrap();
        assert!(r.ready(old).is_err());
        r.ready(new).unwrap();
    }
    #[test]
    fn failure_preserves_recovery_and_requires_cleanup() {
        let mut r = Recorder::default();
        let s = r.begin(&request()).unwrap();
        let error = RecorderError {
            code: ErrorCode::DiskFull,
            message: "Disk is full.".into(),
            recovery: RecoveryStatus::Recoverable,
            action: "Free disk space and recover the session.".into(),
        };
        r.fail(s, error.clone()).unwrap();
        assert_eq!(r.state(), &RecorderState::Failed(error));
        assert!(r.begin(&request()).is_err());
        r.reset_after_cleanup().unwrap();
        assert!(
            r.fail(s, RecorderError::unsupported("late callback"))
                .is_err()
        );
    }
    #[test]
    fn idle_commands_never_claim_recording() {
        let mut r = Recorder::default();
        assert!(r.pause().is_err());
        assert!(r.resume().is_err());
        assert!(r.stop().is_err());
        assert!(r.reset_after_cleanup().is_err());
        assert!(!foundation_snapshot().capabilities.can_record);
    }
}
