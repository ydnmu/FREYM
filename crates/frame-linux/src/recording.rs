use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use frame_audio::{AudioBackend, AudioPipeline};
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{
    AudioRoute, CaptureTarget, Dimensions, EncoderPreference, ErrorCode, FpsMode, RecorderError,
    RecordingArtifact, RecordingRequest, RecoveryStatus, Resolution,
};
use frame_encode::EncodedPacket;
use frame_encode::audio_pipeline::{EncodedAudioDrainPoll, EncodedAudioPipeline};
use frame_mux::{AvAudioTrack, MkvAvWriter, RecordingSession, SegmentState, SessionManifest};
use frame_settings::{EncoderCandidate, EncoderLimits, NominalRate, PlanInputs, SessionPlan};

use crate::{
    PORTAL_PICKER_ID, PlatformBackend, error,
    portal::PortalSession,
    video::{EncoderPath, VideoStream, probe_encoder_for_codec},
};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const PENDING_BYTES: usize = 32 * 1024 * 1024;
const PENDING_PACKETS: usize = 96;
/// A silent or static source cannot indefinitely hold packets from the other
/// track. A packet arriving more than this late after the watermark fails in
/// the common writer instead of being silently dropped or reordered.
const INTERLEAVE_LATENCY_NS: u64 = 500_000_000;

pub struct LinuxRecordingConfig {
    pub request: RecordingRequest,
    pub output: PathBuf,
    pub cancel_start: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingProgress {
    Recording,
    Drained,
}

/// One control-thread owner. GStreamer owns the PipeWire and VA buffers until
/// each appsink sample is copied into a bounded owned packet. The common Opus
/// pipeline and Matroska writer own the remainder of the media path.
pub struct LinuxRecording {
    portal: PortalSession,
    video: VideoStream,
    audio: Option<
        EncodedAudioPipeline<frame_audio::RecoveringAudioSession<crate::audio::LinuxAudioSession>>,
    >,
    writer: Option<MkvAvWriter>,
    session_index: RecordingSession,
    metadata: Option<frame_metadata::MetadataRecorder>,
    clock: Arc<MasterClock>,
    origin: MediaTimestamp,
    plan: SessionPlan,
    pending: Vec<EncodedPacket>,
    pending_bytes: usize,
    latest_video: Option<u64>,
    latest_audio: Option<u64>,
    last_video: Option<u64>,
    video_frames: u64,
    completed_frames: u64,
    draining: bool,
    stop_at: Option<MediaTimestamp>,
    audio_finished: bool,
    audio_padding: Vec<u16>,
    drained: bool,
    failed: bool,
    failure: Option<RecorderError>,
}

impl LinuxRecording {
    pub fn audio_source_lifecycle(&self) -> Vec<frame_audio::SourceLifecycleSnapshot> {
        self.audio
            .as_ref()
            .map_or_else(Vec::new, |audio| audio.source_lifecycle().collect())
    }

    pub fn set_audio_muted(
        &mut self,
        id: &frame_contracts::AudioSourceId,
        muted: bool,
    ) -> Result<(), RecorderError> {
        self.audio.as_mut().map_or(Ok(()), |audio| {
            if audio
                .tracks()
                .iter()
                .any(|track| track.sources.contains(id))
            {
                audio.set_muted(id, muted)
            } else {
                Ok(())
            }
        })
    }

    pub fn start(config: LinuxRecordingConfig) -> Result<Self, RecorderError> {
        let request = &config.request;
        request.audio_encoding.validate()?;
        let CaptureTarget::Display(id) = &request.target else {
            return Err(error(
                ErrorCode::NotImplemented,
                "Linux supports only the portal screen picker target.",
                "Choose the system screen picker.",
            ));
        };
        if id.0 != PORTAL_PICKER_ID
            || !matches!(request.resolution, Resolution::Native)
            || !matches!(request.fps_mode, FpsMode::AutoMatchSource)
            || (request.encoder != EncoderPreference::Auto
                && !matches!(&request.encoder, EncoderPreference::Explicit(id) if id == "linux-vaapi-av1-shm-copy"))
            || request.camera.is_some()
        {
            return Err(error(
                ErrorCode::NotImplemented,
                "Linux currently records the portal-selected screen at native size and source pace with VA-API H.264 or explicit AV1; camera overlay, resizing and fixed FPS are unavailable.",
                "Use the portal picker, native resolution, source FPS and automatic encoder.",
            ));
        }
        if config.output.exists() || manifest_path(&config.output).exists() {
            return Err(error(
                ErrorCode::InvalidOutputPath,
                "The MKV output or its session manifest already exists.",
                "Choose a new file name.",
            ));
        }
        let av1 = matches!(&request.encoder, EncoderPreference::Explicit(id) if id == "linux-vaapi-av1-shm-copy");
        let selected_path = probe_encoder_for_codec(av1)?.ok_or_else(|| {
            error(
                ErrorCode::EncoderUnavailable,
                "The requested VA codec/profile/NV12/CBR combination is unavailable.",
                "Use automatic H.264 selection on a supported VA device.",
            )
        })?;
        let cancel_start = Arc::clone(&config.cancel_start);
        let portal = PortalSession::open(Arc::clone(&cancel_start))?;
        if cancel_start.load(Ordering::Acquire) {
            return Err(start_cancelled());
        }
        let (width, height) = portal.size.ok_or_else(|| {
            error(
                ErrorCode::SourceLost,
                "The selected portal stream did not report its geometry.",
                "Choose another screen or update the portal backend.",
            )
        })?;
        if width == 0 || height == 0 {
            return Err(error(
                ErrorCode::SourceLost,
                "The selected portal screen has empty geometry.",
                "Choose another screen.",
            ));
        }
        if u64::from(width) * u64::from(height) * 4 > 32 * 1024 * 1024 {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "The selected portal screen exceeds the 32 MiB raw-frame budget of the SHM copy path.",
                "Choose a smaller screen, or use a backend with DMABuf import.",
            ));
        }
        // Conservative candidate bound: the requested geometry only. The VA
        // element was opened in Ready by probe; Playing with this exact geometry
        // is still required below before the plan is accepted.
        let candidates = [EncoderCandidate {
            id: selected_path.capability().id,
            codec: selected_path.capability().codec,
            hardware: true,
            limits: EncoderLimits {
                max_width: width,
                max_height: height,
                cbr: true,
                vbr: false,
            },
        }];
        // The portal supplies timestamps but no declared refresh rate. 30/1 is
        // solely an encoder configuration rate, not a claimed capture/source FPS.
        let rate = NonZeroU32::new(30).unwrap();
        let nominal_rate = NominalRate::new(rate, NonZeroU32::new(1).unwrap());
        let plan = SessionPlan::freeze(frame_settings::policy::plan_optimized_recording(
            &PlanInputs {
                quality: request.quality,
                resolution: request.resolution,
                fps_mode: request.fps_mode,
                encoder: request.encoder.clone(),
                native: Dimensions { width, height },
                nominal_rate,
                candidates: &candidates,
            },
            request.optimization.as_ref(),
            if av1 { "av1" } else { "h264" },
            &[],
        )?);
        frame_settings::validation::validate_applied_plan(request, plan.plan())?;
        let clock = Arc::new(MasterClock::default());
        let origin = clock.now();
        let mut video = VideoStream::start(
            &portal,
            Arc::clone(&clock),
            plan.plan().bitrate_bps,
            plan.plan().gop_frames,
            selected_path,
        )?;
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        let first = loop {
            if cancel_start.load(Ordering::Acquire) {
                return Err(start_cancelled());
            }
            if portal.revoked() {
                return Err(error(
                    ErrorCode::PermissionDenied,
                    "The ScreenCast permission was revoked during startup.",
                    "Grant screen sharing again.",
                ));
            }
            if let Some(packet) = video.poll()? {
                break packet;
            }
            if Instant::now() >= deadline {
                return Err(error(
                    ErrorCode::SourceLost,
                    "The selected portal stream produced no encoded frame within 10 seconds.",
                    "Check PipeWire SHM negotiation and the VA-API driver.",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if video.width != width || video.height != height {
            return Err(error(
                ErrorCode::SourceLost,
                "The negotiated stream dimensions differ from the portal selection.",
                "Choose the screen again.",
            ));
        }
        let stream_info = video.stream_info(&first, rate)?;
        let routes: Vec<_> = request
            .audio_routes
            .iter()
            .filter(|r| r.route != AudioRoute::Ignore)
            .cloned()
            .collect();
        let mut audio = if routes.is_empty() {
            None
        } else {
            let available = PlatformBackend.enumerate_audio_sources()?;
            for route in &routes {
                if !available.iter().any(|s| s.id == route.source_id) {
                    return Err(error(
                        ErrorCode::SourceLost,
                        format!(
                            "Selected audio source {} is unavailable.",
                            route.source_id.0
                        ),
                        "Refresh audio sources and choose a connected source.",
                    ));
                }
            }
            let pipeline = AudioPipeline::start(
                &mut frame_audio::NormalizingBackend::with_policy(
                    PlatformBackend,
                    frame_audio::AudioContinuityPolicy::from_environment()?,
                ),
                &routes,
                Arc::clone(&clock),
                origin,
            )?;
            Some(EncodedAudioPipeline::with_settings(
                pipeline,
                origin,
                request.audio_encoding,
            )?)
        };
        let tracks = if let Some(audio) = audio.as_mut() {
            audio
                .tracks()
                .iter()
                .enumerate()
                .map(|(i, plan)| {
                    AvAudioTrack::from_plan(plan, audio.pre_skip(i).expect("planned Opus encoder"))
                        .and_then(|track| track.with_channels(audio.track_channels(i)))
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        if cancel_start.load(Ordering::Acquire) {
            return Err(start_cancelled());
        }
        let indexed_writer = (|| {
            let mut index = RecordingSession::create(&manifest_path(&config.output))?;
            let writer = index.create_av_segment(&stream_info, &tracks, origin, true)?;
            Ok::<_, RecorderError>((index, writer))
        })();
        let (session_index, writer) = match indexed_writer {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(audio) = audio.as_mut() {
                    let _ = audio.stop();
                }
                return Err(error);
            }
        };
        let first_bytes = first.data.len();
        let mut recording = Self {
            portal,
            video,
            audio,
            writer: Some(writer),
            session_index,
            metadata: None,
            clock,
            origin,
            plan,
            latest_video: Some(first.pts.0),
            latest_audio: None,
            last_video: None,
            pending: vec![first],
            pending_bytes: first_bytes,
            video_frames: 1,
            completed_frames: 1,
            draining: false,
            stop_at: None,
            audio_finished: routes.is_empty(),
            audio_padding: Vec::new(),
            drained: false,
            failed: false,
            failure: None,
        };
        if std::env::var_os("FREYM_RECORD_METADATA").is_some_and(|value| value == "1") {
            if let Err(error) =
                recording.enable_metadata(frame_metadata::MetadataOptions::default())
            {
                let _ = recording.abort();
                return Err(error);
            }
        }
        Ok(recording)
    }

    pub fn plan(&self) -> &SessionPlan {
        &self.plan
    }
    /// Explicit local lifecycle metadata; portal/SCK interaction hooks are unavailable.
    pub fn enable_metadata(
        &mut self,
        options: frame_metadata::MetadataOptions,
    ) -> Result<(), RecorderError> {
        if self.metadata.is_some() || self.draining {
            return Err(RecorderError::invalid("Enable metadata once before Stop."));
        }
        let metadata = frame_metadata::MetadataRecorder::start(
            &self.session_index.metadata_path(),
            self.clock.clone(),
            frame_clock::PauseTimeline::default(),
            frame_metadata::Header {
                version: frame_metadata::SCHEMA_VERSION,
                origin_ns: self.session_index.snapshot()?.segments[0].origin_ns,
                cursor_baked: None,
                capabilities: frame_metadata::Capabilities::default(),
            },
            options,
        )
        .map_err(|error| RecorderError::invalid(format!("Local metadata: {error}")))?;
        metadata
            .lifecycle(frame_metadata::Lifecycle::Started)
            .map_err(|error| RecorderError::invalid(error.to_string()))?;
        self.session_index.attach_metadata()?;
        self.metadata = Some(metadata);
        Ok(())
    }
    fn finish_metadata(&mut self, completed: bool) -> Result<(), RecorderError> {
        if let Some(mut metadata) = self.metadata.take() {
            metadata
                .finish(completed)
                .map_err(|error| RecorderError::invalid(format!("Local metadata: {error}")))?;
        }
        Ok(())
    }

    pub fn session_manifest(&self) -> Result<SessionManifest, RecorderError> {
        self.session_index.snapshot()
    }
    pub fn encoder_device(&self) -> Option<&crate::DrmDeviceIdentity> {
        self.video.encoder_device.as_ref()
    }
    pub fn dma_buf_blocker(&self) -> Option<crate::DmaBufBlocker> {
        self.video.dma_buf_blocker
    }
    pub fn encoder_path(&self) -> EncoderPath {
        self.video.path
    }
    pub fn dma_buf_format(&self) -> Option<crate::DmaDrmFormat> {
        self.video
            .import_state
            .as_ref()?
            .lock()
            .ok()?
            .format
            .clone()
    }
    pub fn dma_buf_capture_device(&self) -> Option<crate::DrmDeviceIdentity> {
        self.video
            .import_state
            .as_ref()?
            .lock()
            .ok()?
            .capture_device
            .clone()
    }
    /// Rejected raw buffers terminate a DMA generation. Before first output
    /// they are startup diagnostics; after output they are a recording error.
    pub fn dma_buf_rejected_buffers(&self) -> u64 {
        self.video.dma_buf_rejected_buffers.saturating_add(
            self.video
                .import_state
                .as_ref()
                .and_then(|state| state.lock().ok().map(|state| state.rejected_buffers))
                .unwrap_or(0),
        )
    }
    pub fn clock(&self) -> Arc<MasterClock> {
        Arc::clone(&self.clock)
    }
    pub fn origin(&self) -> MediaTimestamp {
        self.origin
    }
    pub fn captured_frames(&self) -> u64 {
        self.video_frames
    }
    pub fn completed_frames(&self) -> u64 {
        self.completed_frames
    }
    pub fn active_timestamp(&self, raw: MediaTimestamp) -> MediaTimestamp {
        raw
    }
    pub fn pause(&mut self) -> Result<(), RecorderError> {
        Err(error(
            ErrorCode::NotImplemented,
            "Linux pause is unavailable.",
            "Use Stop to finish the recording.",
        ))
    }
    pub fn resume(&mut self) -> Result<(), RecorderError> {
        Err(error(
            ErrorCode::NotImplemented,
            "Linux resume is unavailable.",
            "Start a new recording.",
        ))
    }

    pub fn poll(&mut self) -> Result<RecordingProgress, RecorderError> {
        let result = self.poll_inner();
        match result {
            // A refused device/configuration change must close every owner,
            // including microphone and portal, before returning the failure.
            Err(error) if !self.failed => Err(self.interrupt_error(error, "poll")),
            other => other,
        }
    }

    fn poll_inner(&mut self) -> Result<RecordingProgress, RecorderError> {
        if self.failed {
            return Err(self
                .failure
                .clone()
                .unwrap_or_else(|| RecorderError::invalid("The Linux recording has failed.")));
        }
        if self.drained {
            return Ok(RecordingProgress::Drained);
        }
        if self.portal.revoked() {
            return Err(error(
                ErrorCode::PermissionDenied,
                "ScreenCast permission was revoked.",
                "Grant screen sharing again.",
            ));
        }
        if let Some(packet) = self.video.poll()? {
            self.video_frames += 1;
            self.completed_frames += 1;
            self.push(packet, true)?;
        }
        if let Some(audio) = self.audio.as_mut() {
            if self.draining {
                match audio.poll_drain()? {
                    EncodedAudioDrainPoll::Packets(packets) => {
                        for packet in packets {
                            self.push(packet, false)?;
                        }
                    }
                    EncodedAudioDrainPoll::Finished(finish) => {
                        for packet in finish.packets {
                            self.push(packet, false)?;
                        }
                        self.audio_padding = finish
                            .tracks
                            .iter()
                            .map(|track| track.discard_padding)
                            .collect();
                        self.audio_finished = true;
                    }
                    EncodedAudioDrainPoll::Pending | EncodedAudioDrainPoll::Drained => {}
                }
            } else {
                audio.poll_sources()?;
                let packets = audio.poll(self.clock.now())?;
                for packet in packets {
                    self.push(packet, false)?;
                }
            }
        }
        self.flush_ready(false)?;
        if self.draining && self.video.drained() && self.audio_finished {
            self.flush_ready(true)?;
            let writer = self.writer.as_mut().ok_or_else(|| {
                error(
                    ErrorCode::Internal,
                    "The recording writer disappeared.",
                    "Report this recording failure.",
                )
            })?;
            // Close the last live video block at the actual Stop boundary.
            // A frame already queued by PipeWire can arrive after Stop; in
            // that case one nominal period is the explicit tail policy.
            let tail = self
                .stop_at
                .and_then(|stop| self.last_video.and_then(|last| stop.0.checked_sub(last)))
                .filter(|duration| *duration > 0)
                .unwrap_or(33_333_334);
            writer.finish_video(tail)?;
            for (index, padding) in self.audio_padding.iter().copied().enumerate() {
                writer.finish_audio(index, padding)?;
            }
            self.drained = true;
            return Ok(RecordingProgress::Drained);
        }
        Ok(RecordingProgress::Recording)
    }

    fn push(&mut self, packet: EncodedPacket, video: bool) -> Result<(), RecorderError> {
        if self.pending.len()
            >= PENDING_PACKETS.max(self.audio.as_ref().map_or(0, |a| a.tracks().len() * 32))
            || self.pending_bytes.saturating_add(packet.data.len()) > PENDING_BYTES
        {
            return Err(error(
                ErrorCode::Internal,
                "The bounded A/V interleave buffer filled before both tracks advanced.",
                "Check PipeWire audio delivery and VA encoder latency.",
            ));
        }
        if video {
            self.latest_video = Some(packet.pts.0);
        } else {
            self.latest_audio = Some(packet.pts.0);
        }
        self.pending_bytes += packet.data.len();
        self.pending.push(packet);
        Ok(())
    }

    fn flush_ready(&mut self, all: bool) -> Result<(), RecorderError> {
        let threshold = if all || self.audio.is_none() {
            u64::MAX
        } else {
            let watermark = self.clock.now().0.saturating_sub(INTERLEAVE_LATENCY_NS);
            match (self.latest_video, self.latest_audio) {
                (Some(video), Some(audio)) => video.min(audio).max(watermark),
                _ => watermark,
            }
        };
        self.pending
            .sort_by_key(|packet| (packet.pts.0, packet.track_id));
        let count = self
            .pending
            .iter()
            .take_while(|packet| packet.pts.0 <= threshold)
            .count();
        for packet in self.pending.drain(..count) {
            self.pending_bytes -= packet.data.len();
            let is_video = packet.track_id == 0;
            let pts = packet.pts.0;
            let writer = self.writer.as_mut().expect("recording writer");
            if is_video {
                writer.write_video(packet)?;
                self.last_video = Some(pts);
            } else {
                writer.write_audio(packet)?;
            }
        }
        Ok(())
    }

    pub fn request_stop(&mut self) -> Result<(), RecorderError> {
        if self.failed {
            return Err(self
                .failure
                .clone()
                .unwrap_or_else(|| RecorderError::invalid("The Linux recording has failed.")));
        }
        let result = self.request_stop_inner();
        result.map_err(|error| self.interrupt_error(error, "stop"))
    }

    fn request_stop_inner(&mut self) -> Result<(), RecorderError> {
        if !self.draining {
            self.draining = true;
            self.stop_at = Some(self.clock.now());
            self.video.begin_drain()?;
            if let Some(audio) = self.audio.as_mut() {
                audio.request_drain()?;
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<RecordingArtifact, RecorderError> {
        if self.failed {
            return Err(self
                .failure
                .clone()
                .unwrap_or_else(|| RecorderError::invalid("The Linux recording has failed.")));
        }
        if !self.drained {
            return Err(error(
                ErrorCode::FinalizationFailed,
                "Video and audio have not drained.",
                "Wait for the recording to finish draining.",
            ));
        }
        if let Err(error) = self.video.stop() {
            self.portal.close();
            return Err(self.interrupt_error(error, "video teardown"));
        }
        self.portal.close();
        let result = self
            .writer
            .take()
            .ok_or_else(|| RecorderError::invalid("The Linux recording was already finalized."))?
            .finalize()
            .and_then(|artifact| {
                if !self.session_index.finish()?.completed {
                    return Err(error(
                        ErrorCode::FinalizationFailed,
                        "The session contains an interrupted segment.",
                        "Recover the session and preserve every original segment.",
                    ));
                }
                Ok(artifact)
            });
        result
            .and_then(|artifact| {
                self.finish_metadata(true)?;
                Ok(artifact)
            })
            .map_err(|error| self.interrupt_error(error, "finalization"))
    }

    pub fn abort(&mut self) -> Result<(), RecorderError> {
        if self.failed && self.writer.is_none() {
            return Ok(());
        }
        if self.writer.is_some() && self.failure.is_none() {
            self.failure = Some(RecorderError::invalid("The Linux recording was aborted."));
        }
        self.abort_with_reason("Linux recording aborted by its owner")
    }

    fn abort_with_reason(&mut self, reason: &str) -> Result<(), RecorderError> {
        let metadata_failure = self.finish_metadata(false).err();
        let mut cleanup_error = match self.session_index.snapshot() {
            Ok(manifest)
                if manifest.segments.last().is_some_and(|segment| {
                    matches!(
                        segment.state,
                        SegmentState::Open | SegmentState::Interrupted
                    )
                }) =>
            {
                self.session_index.record_interruption_reason(reason).err()
            }
            Err(error) => Some(error),
            _ => None,
        };
        // Preserve the last complete-element prefix without finishing an unknown
        // video tail or inventing Opus trim after a native failure.
        if let Some(writer) = self.writer.as_mut()
            && writer.stats().video_packets > 0
            && let Err(error) = writer.checkpoint()
            && cleanup_error.is_none()
        {
            cleanup_error = Some(error);
        }
        self.failed = true;
        if let Err(error) = self.video.stop()
            && cleanup_error.is_none()
        {
            cleanup_error = Some(error);
        }
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.stop()
            && cleanup_error.is_none()
        {
            cleanup_error = Some(error);
        }
        self.portal.close();
        self.writer.take();
        self.pending.clear();
        self.pending_bytes = 0;
        cleanup_error.or(metadata_failure).map_or(Ok(()), Err)
    }

    fn interrupt_error(&mut self, mut failure: RecorderError, stage: &str) -> RecorderError {
        let reason = format!(
            "Linux {stage}: {:?}: {}; observed_video_packets={}; pending_packets={}; pending_bytes={}",
            failure.code, failure.message, self.video_frames, self.pending.len(), self.pending_bytes,
        ).chars().take(1000).collect::<String>();
        if let Err(cleanup) = self.abort_with_reason(&reason) {
            failure
                .message
                .push_str(&format!("; interruption cleanup: {}", cleanup.message));
        }
        failure.recovery = match self.session_index.snapshot() {
            Ok(manifest)
                if manifest.segments.iter().any(|segment| {
                    segment.checkpoint.is_some() || segment.state == SegmentState::Closed
                }) =>
            {
                RecoveryStatus::Recoverable
            }
            _ => RecoveryStatus::Unknown,
        };
        failure.action = format!(
            "Recover the session at {}; preserve its original media files.",
            self.session_index.manifest_path().display(),
        );
        self.failure = Some(failure.clone());
        failure
    }
}

fn manifest_path(output: &std::path::Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".freym-session");
    PathBuf::from(path)
}

impl Drop for LinuxRecording {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.abort();
        }
    }
}

fn start_cancelled() -> RecorderError {
    error(
        ErrorCode::InvalidTransition,
        "Recording preparation was stopped before the Linux media owner became ready.",
        "Start a new recording when ready.",
    )
}
