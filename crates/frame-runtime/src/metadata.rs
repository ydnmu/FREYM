//! Control-thread ownership of the optional sidecar and independent native observer.
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline};
use frame_contracts::{CaptureTarget, ErrorCode, RecorderError, RecoveryStatus};
use frame_metadata::{
    EventKind, Header, Lifecycle, MetadataOptions, MetadataRecorder, SCHEMA_VERSION,
};
use std::sync::{Arc, Mutex};

pub(crate) fn error(error: std::io::Error) -> RecorderError {
    RecorderError { code: ErrorCode::FinalizationFailed, message: format!("Local interaction metadata: {error}"), recovery: RecoveryStatus::Unknown,
        action: "Preserve the session manifest, media and metadata sidecar; recover the valid prefix locally.".into() }
}

pub struct RecordingMetadata {
    recorder: MetadataRecorder,
    observer: frame_windows::metadata_observer::WindowsInteractionObserver,
    header: Header,
    target: CaptureTarget,
}
impl RecordingMetadata {
    pub fn start(
        index: &mut frame_mux::RecordingSession,
        clock: Arc<MasterClock>,
        timeline: Arc<Mutex<PauseTimeline>>,
        origin: MediaTimestamp,
        target: CaptureTarget,
        cursor_baked: bool,
        options: MetadataOptions,
    ) -> Result<Self, RecorderError> {
        let snapshot = timeline
            .lock()
            .map_err(|_| error(std::io::Error::other("Media timeline lock failed.")))?
            .clone();
        let header = Header {
            version: SCHEMA_VERSION,
            origin_ns: origin.0,
            cursor_baked: Some(cursor_baked),
            capabilities: frame_windows::metadata_observer::interaction_capabilities(&target),
        };
        let recorder = MetadataRecorder::start(
            &index.metadata_path(),
            clock,
            snapshot,
            header.clone(),
            options,
        )
        .map_err(error)?;
        recorder.lifecycle(Lifecycle::Started).map_err(error)?;
        index.attach_metadata()?;
        let observer = frame_windows::metadata_observer::WindowsInteractionObserver::start(
            target.clone(),
            recorder.ingress(),
        )
        .map_err(error)?;
        Ok(Self {
            recorder,
            observer,
            header,
            target,
        })
    }
    pub fn enable_rolling(&mut self, history_ns: u64) -> Result<(), RecorderError> {
        self.observer.stop().map_err(error)?;
        self.recorder
            .switch_to_rolling(self.header.clone(), history_ns)
            .map_err(error)?;
        self.observer = frame_windows::metadata_observer::WindowsInteractionObserver::start(
            self.target.clone(),
            self.recorder.ingress(),
        )
        .map_err(error)?;
        Ok(())
    }
    pub fn rolling(&self) -> Option<frame_metadata::RollingMetadata> {
        self.recorder.rolling()
    }
    pub fn boundary(
        &self,
        raw: MediaTimestamp,
        mapped: MediaTimestamp,
        state: Lifecycle,
    ) -> Result<(), RecorderError> {
        self.recorder
            .boundary(raw, mapped, EventKind::Lifecycle { state })
            .map_err(error)
    }
    pub fn transition(
        &self,
        media: &mut PauseTimeline,
        state: Lifecycle,
    ) -> Result<(), RecorderError> {
        self.recorder.transition(media, state).map_err(error)
    }
    pub fn stopping(&mut self) -> Result<(), RecorderError> {
        let stopped = self.observer.stop().map_err(error);
        let boundary = self.recorder.lifecycle(Lifecycle::Stopping).map_err(error);
        stopped.and(boundary)
    }
    pub fn segment(&self, origin: MediaTimestamp) -> Result<(), RecorderError> {
        // Segment origin is the exact writer origin, independent of observation time.
        if !self.recorder.ingress().try_emit(EventKind::Segment {
            origin_ns: origin.0,
        }) {
            self.recorder.check().map_err(error)?;
        }
        Ok(())
    }
    pub fn finish(&mut self, completed: bool) -> Result<(), RecorderError> {
        let stopped = self.observer.stop().map_err(error);
        let result = self
            .recorder
            .finish(completed && stopped.is_ok())
            .map_err(error);
        stopped.and(result)
    }
}
impl Drop for RecordingMetadata {
    fn drop(&mut self) {
        let _ = self.finish(false);
    }
}

pub(crate) fn enabled() -> bool {
    std::env::var_os("FREYM_RECORD_METADATA").is_some_and(|value| value == "1")
}
