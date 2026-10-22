//! One macOS recording owner driven by the existing desktop control thread.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use frame_audio::{AudioPipeline, NormalizingBackend, RecoveringAudioSession};
use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame};
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{
    AudioRouteSelection, CaptureTarget, ErrorCode, RecorderError, RecordingArtifact, RecoveryStatus,
};
use frame_encode::audio_pipeline::{EncodedAudioDrainPoll, EncodedAudioPipeline};
use frame_encode::{
    EncodedPacket, EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, nominal_duration,
};
use frame_macos::{
    MacAudioSession, MacCaptureSession, MacEncoderConfig, MacH264Encoder, MacSurface,
    PlatformBackend,
};
use frame_mux::{AvAudioTrack, MkvAvWriter, RecordingSession, SegmentState, SessionManifest};

type Audio = EncodedAudioPipeline<RecoveringAudioSession<MacAudioSession>>;
const PREPARE_LIMIT: Duration = Duration::from_secs(5);
const DRAIN_LIMIT: Duration = Duration::from_secs(8);
const INTERLEAVE_DELAY_NS: u64 = 500_000_000;
const MAX_INTERLEAVE_PACKETS: usize = 512;
const MAX_INTERLEAVE_BYTES: usize = 64 * 1024 * 1024;

fn failure(code: ErrorCode, message: impl Into<String>) -> RecorderError {
    RecorderError {
        code,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Preserve the incomplete file and retry after refreshing sources.".into(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Recording,
    Drained,
}

pub struct MacRecording {
    capture: Option<MacCaptureSession>,
    encoder: Option<MacH264Encoder>,
    audio: Option<Audio>,
    writer: Option<MkvAvWriter>,
    session_index: RecordingSession,
    metadata: Option<frame_metadata::MetadataRecorder>,
    clock: Arc<MasterClock>,
    nominal_tail: u64,
    pending: Option<EncoderInput<CapturedFrame<MacSurface>>>,
    interleave: BTreeMap<(u64, u8, u32, u64), EncodedPacket>,
    interleave_bytes: usize,
    tracks_seen: Vec<bool>,
    last_written: Option<(u64, u8, u32)>,
    sequence: u64,
    stop_requested: bool,
    encoder_drained: bool,
    audio_finished: bool,
    audio_trim: Vec<u16>,
    drain_started: Option<Instant>,
    capture_frames: u64,
    encoded_frames: u64,
    closed: bool,
    failure: Option<RecorderError>,
}

impl MacRecording {
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

    pub fn start(
        target: CaptureTarget,
        config: MacEncoderConfig,
        routes: &[AudioRouteSelection],
        clock: Arc<MasterClock>,
        origin: MediaTimestamp,
        output: &Path,
    ) -> Result<Self, RecorderError> {
        if output.exists() || manifest_path(output).exists() {
            return Err(failure(
                ErrorCode::InvalidOutputPath,
                "Output file or session manifest already exists.",
            ));
        }
        let mut backend = PlatformBackend {
            application_display_id: match &target {
                CaptureTarget::Display(id) => id.0.parse().ok(),
                _ => None,
            },
        };
        let mut capture = backend.start_with_rate(
            &target,
            config.rate_num.get(),
            config.rate_den.get(),
            CaptureContext {
                clock: Arc::clone(&clock),
                cancellation: CaptureCancellation::default(),
            },
        )?;
        let first = {
            let deadline = Instant::now() + PREPARE_LIMIT;
            loop {
                if let Some(frame) = capture.try_frame()? {
                    break frame;
                }
                if Instant::now() >= deadline {
                    return Err(failure(
                        ErrorCode::SourceLost,
                        "ScreenCaptureKit delivered no frame within five seconds.",
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        if first.width != config.width || first.height != config.height {
            return Err(failure(
                ErrorCode::InvalidRequest,
                format!(
                    "Capture size {}x{} differs from the planned encoder size {}x{}.",
                    first.width, first.height, config.width, config.height
                ),
            ));
        }
        let mut encoder = MacH264Encoder::open(config)?;
        let tail = nominal_duration(config.rate_num, config.rate_den);
        match encoder.submit(EncoderInput {
            input: first,
            duration_ns: tail,
        })? {
            SubmitOutcome::Accepted => {}
            SubmitOutcome::Backpressure(_) => {
                return Err(failure(
                    ErrorCode::EncoderUnavailable,
                    "VideoToolbox rejected its first frame under pressure.",
                ));
            }
        }
        let first_packet = {
            let deadline = Instant::now() + PREPARE_LIMIT;
            loop {
                if let EncoderPoll::Packet(packet) = encoder.try_packet()? {
                    break packet;
                }
                if Instant::now() >= deadline {
                    return Err(failure(
                        ErrorCode::EncoderUnavailable,
                        "VideoToolbox delivered no H.264 packet within five seconds.",
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        if !encoder.refresh_config() {
            return Err(failure(
                ErrorCode::EncoderUnavailable,
                "VideoToolbox delivered no H.264 SPS/PPS configuration.",
            ));
        }
        if !first_packet.keyframe
            || !frame_mux::independently_decodable_start(
                encoder.stream_info().bitstream_format,
                &first_packet.data,
            )
        {
            return Err(failure(
                ErrorCode::EncoderUnavailable,
                "The first VideoToolbox packet is not independently decodable.",
            ));
        }
        let mut audio = if routes.is_empty() {
            None
        } else {
            let mut normalized = NormalizingBackend::with_policy(
                backend,
                frame_audio::AudioContinuityPolicy::from_environment()?,
            );
            let pipeline =
                AudioPipeline::start(&mut normalized, routes, Arc::clone(&clock), origin)?;
            Some(EncodedAudioPipeline::new(pipeline, origin)?)
        };
        let mut tracks = Vec::new();
        if let Some(ref pipeline) = audio {
            for (index, plan) in pipeline.tracks().iter().enumerate() {
                tracks.push(
                    AvAudioTrack::from_plan(
                        plan,
                        pipeline.pre_skip(index).ok_or_else(|| {
                            failure(ErrorCode::Internal, "Opus pre-skip is missing.")
                        })?,
                    )?
                    .with_channels(pipeline.track_channels(index))?,
                );
            }
        }
        let (session_index, writer) =
            match create_session_writer(output, encoder.stream_info(), &tracks, origin) {
                Ok(prepared) => prepared,
                Err(err) => {
                    if let Some(pipeline) = audio.as_mut() {
                        let _ = pipeline.stop();
                    }
                    return Err(err);
                }
            };
        let mut recording = Self {
            capture: Some(capture),
            encoder: Some(encoder),
            audio,
            writer: Some(writer),
            session_index,
            metadata: None,
            clock,
            nominal_tail: tail.get(),
            pending: None,
            interleave: BTreeMap::new(),
            interleave_bytes: 0,
            tracks_seen: vec![false; tracks.len() + 1],
            last_written: None,
            sequence: 0,
            stop_requested: false,
            encoder_drained: false,
            audio_finished: routes.is_empty(),
            audio_trim: Vec::new(),
            drain_started: None,
            capture_frames: 1,
            encoded_frames: 1,
            closed: false,
            failure: None,
        };
        recording.queue(first_packet, true)?;
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

    pub fn counts(&self) -> (u64, u64) {
        (self.capture_frames, self.encoded_frames)
    }
    /// Explicit local lifecycle metadata; portal/SCK interaction hooks are unavailable.
    pub fn enable_metadata(
        &mut self,
        options: frame_metadata::MetadataOptions,
    ) -> Result<(), RecorderError> {
        if self.metadata.is_some() || self.stop_requested {
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
    pub fn active_timestamp(&self, raw: MediaTimestamp) -> MediaTimestamp {
        raw
    }
    pub fn pause(&mut self) -> Result<(), RecorderError> {
        Err(failure(
            ErrorCode::NotImplemented,
            "Pause is unavailable on macOS.",
        ))
    }
    pub fn resume(&mut self) -> Result<(), RecorderError> {
        Err(failure(
            ErrorCode::NotImplemented,
            "Resume is unavailable on macOS.",
        ))
    }
    pub fn request_stop(&mut self) -> Result<(), RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        if let Some(metadata) = &self.metadata {
            metadata
                .lifecycle(frame_metadata::Lifecycle::Stopping)
                .map_err(|error| RecorderError::invalid(error.to_string()))?;
        }
        let result = self.request_stop_inner();
        result.map_err(|error| self.interrupt_error(error, "stop"))
    }

    fn request_stop_inner(&mut self) -> Result<(), RecorderError> {
        if !self.stop_requested {
            self.stop_requested = true;
            self.drain_started = Some(Instant::now());
            if let Some(capture) = self.capture.as_mut() {
                capture.stop()?;
            }
            if let Some(audio) = self.audio.as_mut() {
                audio.request_drain()?;
            }
        }
        Ok(())
    }
    fn queue(&mut self, packet: EncodedPacket, video: bool) -> Result<(), RecorderError> {
        let track = if video {
            0
        } else {
            packet.track_id as usize + 1
        };
        let Some(seen) = self.tracks_seen.get_mut(track) else {
            return Err(failure(
                ErrorCode::InvalidRequest,
                "Packet belongs to an undeclared track.",
            ));
        };
        *seen = true;
        if self.interleave.len() >= MAX_INTERLEAVE_PACKETS.max(self.tracks_seen.len() * 32)
            || packet.data.len() > MAX_INTERLEAVE_BYTES.saturating_sub(self.interleave_bytes)
        {
            return Err(failure(
                ErrorCode::Internal,
                "A/V interleave exceeded its frozen plan or 64-MiB budget.",
            ));
        }
        self.sequence += 1;
        self.interleave_bytes += packet.data.len();
        self.interleave.insert(
            (
                packet.pts.0,
                if video { 0 } else { 1 },
                packet.track_id,
                self.sequence,
            ),
            packet,
        );
        Ok(())
    }
    fn write_ready(&mut self, all: bool) -> Result<(), RecorderError> {
        if !all && self.tracks_seen.iter().any(|seen| !seen) {
            return Ok(());
        }
        let frontier = self.clock.now().0.saturating_sub(INTERLEAVE_DELAY_NS);
        for _ in 0..64 {
            let Some((&key, _)) = self.interleave.first_key_value() else {
                break;
            };
            if !all && key.0 > frontier {
                break;
            }
            let packet = self.interleave.remove(&key).expect("front key exists");
            self.interleave_bytes -= packet.data.len();
            let position = (key.0, key.1, key.2);
            if self.last_written.is_some_and(|last| position < last) {
                return Err(failure(
                    ErrorCode::FinalizationFailed,
                    "A/V packet arrived behind the written container frontier.",
                ));
            }
            self.last_written = Some(position);
            let writer = self
                .writer
                .as_mut()
                .ok_or_else(|| failure(ErrorCode::Internal, "Recording writer closed early."))?;
            if key.1 == 0 {
                writer.write_video(packet)?;
            } else {
                writer.write_audio(packet)?;
            }
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Phase, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        let result = self.poll_inner();
        result.map_err(|error| self.interrupt_error(error, "poll"))
    }

    fn poll_inner(&mut self) -> Result<Phase, RecorderError> {
        if self.closed {
            return Ok(Phase::Drained);
        }
        if self.stop_requested
            && self
                .drain_started
                .is_some_and(|at| at.elapsed() > DRAIN_LIMIT)
        {
            return Err(failure(
                ErrorCode::FinalizationFailed,
                "Native encoder or audio drain timed out.",
            ));
        }
        if !self.stop_requested {
            if self.pending.is_none()
                && let Some(capture) = self.capture.as_mut()
            {
                if let Some(frame) = capture.try_frame()? {
                    self.capture_frames += 1;
                    self.pending = Some(EncoderInput {
                        input: frame,
                        duration_ns: std::num::NonZeroU64::new(self.nominal_tail)
                            .expect("positive nominal period"),
                    });
                }
            }
        }
        if let Some(pending) = self.pending.take() {
            match self
                .encoder
                .as_mut()
                .expect("encoder active")
                .submit(pending)?
            {
                SubmitOutcome::Accepted => {}
                SubmitOutcome::Backpressure(input) => self.pending = Some(input),
            }
        }
        for _ in 0..8 {
            match self
                .encoder
                .as_mut()
                .expect("encoder active")
                .try_packet()?
            {
                EncoderPoll::Packet(packet) => {
                    self.encoded_frames += 1;
                    self.queue(packet, true)?;
                }
                EncoderPoll::Pending => break,
                EncoderPoll::Drained => {
                    let (accepted, completed) =
                        self.encoder.as_ref().expect("encoder active").counts();
                    validate_completed_inputs(accepted, completed)?;
                    self.encoder_drained = true;
                    break;
                }
            }
        }
        if !self.stop_requested {
            for _ in 0..16 {
                let Some(audio) = self.audio.as_mut() else {
                    break;
                };
                let accepted = audio.poll_sources()?;
                let packets = audio.poll(self.clock.now())?;
                for packet in packets {
                    self.queue(packet, false)?;
                }
                if accepted == 0 {
                    break;
                }
            }
        } else {
            if self.pending.is_none() {
                self.encoder
                    .as_mut()
                    .expect("encoder active")
                    .begin_drain()?;
            }
            if !self.audio_finished {
                let Some(audio) = self.audio.as_mut() else {
                    return Err(failure(
                        ErrorCode::Internal,
                        "Audio owner disappeared during drain.",
                    ));
                };
                match audio.poll_drain()? {
                    EncodedAudioDrainPoll::Packets(packets) => {
                        for packet in packets {
                            self.queue(packet, false)?;
                        }
                    }
                    EncodedAudioDrainPoll::Finished(finish) => {
                        for packet in finish.packets {
                            self.queue(packet, false)?;
                        }
                        self.audio_trim = finish
                            .tracks
                            .into_iter()
                            .map(|track| track.discard_padding)
                            .collect();
                        self.audio_finished = true;
                    }
                    EncodedAudioDrainPoll::Pending | EncodedAudioDrainPoll::Drained => {}
                }
            }
        }
        self.write_ready(self.stop_requested && self.encoder_drained && self.audio_finished)?;
        if self.stop_requested
            && self.encoder_drained
            && self.audio_finished
            && self.interleave.is_empty()
        {
            self.closed = true;
            return Ok(Phase::Drained);
        }
        Ok(Phase::Recording)
    }
    pub fn finish(&mut self) -> Result<RecordingArtifact, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        if !self.closed {
            return Err(failure(
                ErrorCode::FinalizationFailed,
                "Recording has not drained.",
            ));
        }
        if self.writer.is_none() {
            return Err(failure(
                ErrorCode::InvalidTransition,
                "Recording was already finalized.",
            ));
        }
        let result = self.finish_inner();
        result.map_err(|error| self.interrupt_error(error, "finalization"))
    }

    fn finish_inner(&mut self) -> Result<RecordingArtifact, RecorderError> {
        self.encoder.as_mut().expect("encoder active").stop()?;
        self.encoder = None;
        if let Some(audio) = self.audio.as_mut() {
            audio.stop()?;
        }
        self.audio = None;
        if let Some(capture) = self.capture.as_mut() {
            capture.stop()?;
        }
        self.capture = None;
        let mut writer = self.writer.take().expect("writer active");
        writer.finish_video(self.nominal_tail)?;
        for (index, trim) in self.audio_trim.iter().copied().enumerate() {
            writer.finish_audio(index, trim)?;
        }
        let artifact = finish_session_writer(&mut self.session_index, writer)?;
        self.finish_metadata(true)?;
        Ok(artifact)
    }
    pub fn abort(&mut self) -> Result<(), RecorderError> {
        if self.writer.is_none()
            && self.encoder.is_none()
            && self.audio.is_none()
            && self.capture.is_none()
        {
            return Ok(());
        }
        if self.writer.is_some() && self.failure.is_none() {
            self.failure = Some(failure(
                ErrorCode::InvalidTransition,
                "Recording was aborted.",
            ));
        }
        self.abort_with_reason("macOS recording aborted by its owner")
    }

    fn abort_with_reason(&mut self, reason: &str) -> Result<(), RecorderError> {
        let metadata_failure = self.finish_metadata(false).err();
        let mut cleanup_error =
            interrupt_session_writer(&mut self.session_index, &mut self.writer, reason).err();
        self.writer = None;
        self.encoder = None;
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.stop()
            && cleanup_error.is_none()
        {
            cleanup_error = Some(error);
        }
        self.audio = None;
        if let Some(capture) = self.capture.as_mut()
            && let Err(error) = capture.stop()
            && cleanup_error.is_none()
        {
            cleanup_error = Some(error);
        }
        self.capture = None;
        self.pending = None;
        self.interleave.clear();
        self.interleave_bytes = 0;
        self.closed = true;
        cleanup_error.or(metadata_failure).map_or(Ok(()), Err)
    }

    fn interrupt_error(&mut self, mut error: RecorderError, stage: &str) -> RecorderError {
        let (accepted, completed) = self
            .encoder
            .as_ref()
            .map(|encoder| encoder.counts())
            .unwrap_or_default();
        let reason = interruption_reason(
            stage,
            &error,
            accepted,
            completed,
            self.pending.is_some(),
            self.interleave.len(),
            self.interleave_bytes,
        );
        if let Err(cleanup) = self.abort_with_reason(&reason) {
            error
                .message
                .push_str(&format!("; interruption cleanup: {}", cleanup.message));
        }
        error.recovery = match self.session_index.snapshot() {
            Ok(manifest)
                if manifest.segments.iter().any(|segment| {
                    segment.checkpoint.is_some() || segment.state == SegmentState::Closed
                }) =>
            {
                RecoveryStatus::Recoverable
            }
            _ => RecoveryStatus::Unknown,
        };
        error.action = format!(
            "Recover the session at {}; preserve its original media files.",
            self.session_index.manifest_path().display(),
        );
        self.failure = Some(error.clone());
        error
    }
}
impl Drop for MacRecording {
    fn drop(&mut self) {
        if self.writer.is_some()
            || self.encoder.is_some()
            || self.audio.is_some()
            || self.capture.is_some()
        {
            let _ = self.abort();
        }
    }
}

fn manifest_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".freym-session");
    PathBuf::from(path)
}

fn create_session_writer(
    output: &Path,
    stream: &frame_encode::EncodedStreamInfo,
    tracks: &[AvAudioTrack],
    origin: MediaTimestamp,
) -> Result<(RecordingSession, MkvAvWriter), RecorderError> {
    let mut index = RecordingSession::create(&manifest_path(output))?;
    let writer = index.create_av_segment(stream, tracks, origin, true)?;
    Ok((index, writer))
}

fn finish_session_writer(
    index: &mut RecordingSession,
    writer: MkvAvWriter,
) -> Result<RecordingArtifact, RecorderError> {
    let artifact = writer.finalize()?;
    if !index.finish()?.completed {
        return Err(failure(
            ErrorCode::FinalizationFailed,
            "The session contains an interrupted segment.",
        ));
    }
    Ok(artifact)
}

fn interrupt_session_writer(
    index: &mut RecordingSession,
    writer: &mut Option<MkvAvWriter>,
    reason: &str,
) -> Result<(), RecorderError> {
    let mut error = match index.snapshot() {
        Ok(manifest)
            if manifest.segments.last().is_some_and(|segment| {
                matches!(
                    segment.state,
                    SegmentState::Open | SegmentState::Interrupted
                )
            }) =>
        {
            index.record_interruption_reason(reason).err()
        }
        Err(error) => Some(error),
        _ => None,
    };
    if let Some(writer) = writer.as_mut()
        && writer.stats().video_packets > 0
        && let Err(checkpoint) = writer.checkpoint()
        && error.is_none()
    {
        error = Some(checkpoint);
    }
    writer.take();
    error.map_or(Ok(()), Err)
}

fn interruption_reason(
    stage: &str,
    error: &RecorderError,
    accepted: u64,
    completed: u64,
    pending_input: bool,
    pending_packets: usize,
    pending_bytes: usize,
) -> String {
    format!(
        "macOS {stage}: {:?}: {}; accepted={accepted}; completed={completed}; outstanding={}; pending_input={pending_input}; pending_packets={pending_packets}; pending_bytes={pending_bytes}",
        error.code, error.message, accepted.saturating_sub(completed),
    ).chars().take(1000).collect()
}

fn validate_completed_inputs(accepted: u64, completed: u64) -> Result<(), RecorderError> {
    if accepted != completed {
        return Err(failure(
            ErrorCode::FinalizationFailed,
            format!(
                "VideoToolbox drained with {accepted} accepted inputs and {completed} completed packets."
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use frame_encode::{BitstreamFormat, EncodedStreamInfo, EncoderCapability, VideoColour};
    use frame_mux::{read_session_manifest, recover_session};
    use std::num::NonZeroU32;

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join("freym-platform-session");
        let dir = root.join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn stream() -> EncodedStreamInfo {
        EncodedStreamInfo {
            capability: EncoderCapability {
                id: "platform-session-fixture".into(),
                codec: "hevc".into(),
                hardware: false,
            },
            width: NonZeroU32::new(64).unwrap(),
            height: NonZeroU32::new(64).unwrap(),
            nominal_rate_num: NonZeroU32::new(1).unwrap(),
            nominal_rate_den: NonZeroU32::new(1).unwrap(),
            bitstream_format: BitstreamFormat::HevcAnnexB,
            colour: Some(VideoColour::BT2020_PQ_LIMITED),
            codec_config: include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.hevc")
                .to_vec(),
            allows_reordering: false,
        }
    }
    fn packet() -> EncodedPacket {
        EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(7_000_000_000),
            dts: MediaTimestamp(7_000_000_000),
            duration_ns: 1_000_000_000,
            keyframe: true,
            data: stream().codec_config,
        }
    }

    #[test]
    fn platform_live_segment_preserves_output_and_durable_session_integrity() {
        let output = scratch().join("recording.capture");
        let (mut index, mut writer) =
            create_session_writer(&output, &stream(), &[], MediaTimestamp(7_000_000_000)).unwrap();
        writer.write_video(packet()).unwrap();
        writer.finish_video(1_000_000_000).unwrap();
        let artifact = finish_session_writer(&mut index, writer).unwrap();
        assert_eq!(
            Path::new(&artifact.path).canonicalize().unwrap(),
            output.canonicalize().unwrap()
        );
        let manifest = read_session_manifest(&manifest_path(&output)).unwrap();
        assert!(manifest.completed);
        let segment = &manifest.segments[0];
        assert_eq!(segment.file_name, "recording.capture");
        assert_eq!(segment.origin_ns, 7_000_000_000);
        assert_eq!(segment.state, SegmentState::Closed);
        assert!(!segment.tracks_ebml.is_empty());
        assert!(segment.file_bytes > 0 && segment.crc64 != 0 && segment.cues > 0);
        assert!(segment.checkpoint.is_some());
        drop(index);
        let recovered = recover_session(
            &manifest_path(&output),
            &output.with_extension("restored.freym-session"),
        )
        .unwrap();
        assert_eq!(recovered.verified_closed_segments, 1);
        assert_eq!(recovered.manifest.segments[0].state, SegmentState::Closed);
        assert_eq!(recovered.manifest.segments[0].crc64, segment.crc64);
        // Recovery retains its own journal instead of claiming original completion.
        assert!(!recovered.manifest.completed);
    }

    #[test]
    fn platform_native_failure_publishes_provenance_and_preserves_recovery_prefix() {
        let output = scratch().join("interrupted.mkv");
        let (mut index, writer) =
            create_session_writer(&output, &stream(), &[], MediaTimestamp(7_000_000_000)).unwrap();
        let mut writer = Some(writer);
        writer.as_mut().unwrap().write_video(packet()).unwrap();
        let reason = interruption_reason(
            "poll",
            &failure(ErrorCode::EncoderUnavailable, "native failure"),
            5,
            1,
            true,
            3,
            4096,
        );
        interrupt_session_writer(&mut index, &mut writer, &reason).unwrap();
        assert!(writer.is_none());
        let manifest = read_session_manifest(&manifest_path(&output)).unwrap();
        assert_eq!(manifest.segments[0].state, SegmentState::Interrupted);
        assert_eq!(
            manifest.segments[0].interruption_reason.as_deref(),
            Some(reason.as_str())
        );
        assert!(
            reason.contains(
                "outstanding=4; pending_input=true; pending_packets=3; pending_bytes=4096"
            )
        );
        assert!(manifest.segments[0].checkpoint.is_some());
        let original = std::fs::read(&output).unwrap();
        drop(index);
        let restored = recover_session(
            &manifest_path(&output),
            &output.with_extension("restored.freym-session"),
        )
        .unwrap();
        assert_eq!(restored.recovered_tails.len(), 1);
        assert!(!restored.manifest.completed);
        assert_eq!(std::fs::read(&output).unwrap(), original);
    }

    #[test]
    fn native_drain_refuses_missing_or_unaccepted_completions() {
        assert!(validate_completed_inputs(3, 3).is_ok());
        assert_eq!(
            validate_completed_inputs(3, 2).unwrap_err().code,
            ErrorCode::FinalizationFailed
        );
        assert!(validate_completed_inputs(3, 4).is_err());
        let reason = interruption_reason(
            "poll",
            &failure(ErrorCode::EncoderUnavailable, "界".repeat(5000)),
            2,
            1,
            false,
            0,
            0,
        );
        assert!(reason.len() <= 4096);
    }
}
