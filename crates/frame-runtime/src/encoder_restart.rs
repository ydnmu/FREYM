//! One qualified encoder replacement at an independent segment boundary.
use frame_capture::CapturedFrame;
use frame_contracts::{ErrorCode, RecorderError, RecordingArtifact, RecoveryStatus};
use frame_encode::{EncodedPacket, EncodedStreamInfo, PacketEncoder};
use frame_windows::{
    EncoderCounterSnapshot, NativeEncoderConfig, WindowsH264Encoder, WindowsSurface,
};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(crate) struct RestartBudget {
    enabled: bool,
    pub alternative_backend: bool,
    pub attempts: u32,
}
impl RestartBudget {
    pub fn from_environment() -> Result<Self, RecorderError> {
        let enabled = match std::env::var("FREYM_ENCODER_SEGMENT_RESTART") {
            Err(std::env::VarError::NotPresent) => false,
            Ok(value) if value == "0" => false,
            Ok(value) if value == "1" => true,
            _ => {
                return Err(RecorderError::invalid(
                    "FREYM_ENCODER_SEGMENT_RESTART accepts only 0 or 1.",
                ));
            }
        };
        Ok(Self {
            enabled,
            alternative_backend: match std::env::var("FREYM_ENCODER_ALTERNATIVE_BACKEND") {
                Err(std::env::VarError::NotPresent) => false,
                Ok(value) if value == "0" => false,
                Ok(value) if value == "1" => true,
                _ => {
                    return Err(RecorderError::invalid(
                        "FREYM_ENCODER_ALTERNATIVE_BACKEND accepts only 0 or 1.",
                    ));
                }
            },
            attempts: 0,
        })
    }
    pub fn begin(
        &mut self,
        live: bool,
        reported: &RecorderError,
        worker: Option<RecorderError>,
    ) -> bool {
        if !self.enabled
            || self.attempts != 0
            || !live
            || !worker.is_some_and(|worker| {
                worker.code == reported.code && worker.message == reported.message
            })
        {
            return false;
        }
        self.attempts += 1;
        true
    }
}

pub(crate) fn manifest_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".freym-session");
    PathBuf::from(path)
}

/// Join/release before reusing the capture device. A sticky primary encode error
/// may be expected; retained native ownership or unproved cleanup forbids restart.
pub(crate) fn release_failed(encoder: &mut WindowsH264Encoder) -> Result<(), RecorderError> {
    let failure = encoder
        .failure()
        .ok_or_else(|| RecorderError::invalid("Only a failed encoder can be restarted."))?;
    let stopped = encoder.stop();
    validate_cleanup(&failure, &stopped, encoder.counters())
}

fn validate_cleanup(
    failure: &RecorderError,
    stopped: &Result<(), RecorderError>,
    counters: EncoderCounterSnapshot,
) -> Result<(), RecorderError> {
    // The expected sticky encode error is compatible with a clean join. A new
    // join/cleanup error (including a worker panic) is not proof of safe reuse,
    // even if the worker did not manage to publish its ownership counters.
    if let Err(cleanup) = stopped
        && (cleanup.code != failure.code || cleanup.message != failure.message)
    {
        return Err(RecorderError {
            message: format!(
                "{} Restart refused after cleanup: {}",
                failure.message, cleanup.message
            ),
            ..failure.clone()
        });
    }
    if counters.retained_native_owner
        || counters.retained_leases != 0
        || counters.leases_kept_alive != 0
        || counters.slots_kept_alive_on_failure != 0
        || counters.unsafe_cleanup_steps != 0
        || counters.queued_inputs != 0
        || counters.inflight != 0
    {
        return Err(RecorderError {
            message: format!(
                "{} Restart refused: native cleanup did not prove safe device reuse.",
                failure.message
            ),
            ..failure.clone()
        });
    }
    counters.identities().map_err(|detail| RecorderError {
        message: format!(
            "{} Restart refused: post-stop frame accounting did not reconcile: {detail}",
            failure.message
        ),
        ..failure.clone()
    })?;
    Ok(())
}

/// This check runs before handing any replacement packet to the mux or replay.
/// A codec keyframe flag cannot certify an independent coded picture by itself.
pub(crate) fn validate_first_packet(
    stream: &EncodedStreamInfo,
    packet: &EncodedPacket,
) -> Result<(), RecorderError> {
    if !packet.keyframe
        || !frame_mux::independently_decodable_start(stream.bitstream_format, &packet.data)
    {
        return Err(RecorderError {
            code: ErrorCode::FinalizationFailed,
            recovery: RecoveryStatus::Recoverable,
            message: "Replacement segment did not begin with an independently decodable picture."
                .into(),
            action: "Preserve the session and recover its independently decodable segments.".into(),
        });
    }
    Ok(())
}

/// Pump the original audio pipeline while native encoder preparation runs. The
/// temporary preparation thread is always joined; no native owner is detached.
pub(crate) fn prepare(
    frame: CapturedFrame<WindowsSurface>,
    config: NativeEncoderConfig,
    id: String,
    alternative_backend: bool,
    mut pump: impl FnMut() -> Result<(), RecorderError>,
) -> Result<
    (
        WindowsH264Encoder,
        CapturedFrame<WindowsSurface>,
        Option<String>,
    ),
    RecorderError,
> {
    let thread = std::thread::Builder::new()
        .name("freym-segment-prepare".into())
        .spawn(move || {
            WindowsH264Encoder::open_segment_replacement(
                config,
                &frame.surface,
                &id,
                alternative_backend,
            )
            .map(|(encoder, declined)| (encoder, frame, declined))
        })
        .map_err(|error| {
            RecorderError::invalid(format!("Could not prepare the new segment: {error}"))
        })?;
    join_preparation(
        thread,
        crate::video_recording::PREPARATION_WATCHDOG,
        &mut pump,
        |(encoder, _, _)| crate::video_recording::stop_preparation_encoder(encoder),
    )
}

/// The budget limits admission of the replacement, not the native join. Keep
/// servicing healthy audio after expiry until the owner can be safely joined.
fn join_preparation<T>(
    thread: std::thread::JoinHandle<Result<T, RecorderError>>,
    budget: std::time::Duration,
    mut pump: impl FnMut() -> Result<(), RecorderError>,
    cleanup: impl FnOnce(&mut T) -> Result<(), RecorderError>,
) -> Result<T, RecorderError> {
    let started = std::time::Instant::now();
    let mut failure = None;
    let mut pump_failed = false;
    while !thread.is_finished() {
        if !pump_failed && let Err(error) = pump() {
            pump_failed = true;
            append_failure(&mut failure, error, "audio service");
        }
        if failure.is_none() && started.elapsed() >= budget {
            failure = Some(preparation_timeout());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if failure.is_none() && started.elapsed() >= budget {
        failure = Some(preparation_timeout());
    }
    let prepared = thread.join().unwrap_or_else(|_| {
        Err(RecorderError::invalid(
            "Segment encoder preparation panicked.",
        ))
    });
    if let Some(mut primary) = failure {
        match prepared {
            Ok(mut prepared) => {
                if let Err(error) = cleanup(&mut prepared) {
                    primary
                        .message
                        .push_str(&format!(" Replacement cleanup failed: {}", error.message));
                }
            }
            Err(error) => {
                primary
                    .message
                    .push_str(&format!(" Encoder preparation failed: {}", error.message));
            }
        }
        return Err(primary);
    }
    prepared
}

fn preparation_timeout() -> RecorderError {
    RecorderError {
        code: ErrorCode::EncoderUnavailable,
        recovery: RecoveryStatus::Unknown,
        message: "Replacement encoder exceeded the preparation budget; native ownership was joined before rejecting it.".into(),
        action: "Recover the existing session segments.".into(),
    }
}

fn append_failure(failure: &mut Option<RecorderError>, error: RecorderError, stage: &str) {
    if let Some(primary) = failure {
        primary
            .message
            .push_str(&format!(" {stage} failed: {}", error.message));
    } else {
        *failure = Some(error);
    }
}

/// Retain the original native accounting and identify the later failed stage.
/// If publication fails, return that secondary error as well; do not claim it
/// reached the atomic manifest. Only a published media checkpoint or closed
/// segment permits a recoverable verdict; an unverified early tail is unknown.
pub(crate) fn record_restart_failure(
    index: Option<&mut frame_mux::RecordingSession>,
    stage: &str,
    preparation_frames_discarded: u64,
    mut failure: RecorderError,
) -> RecorderError {
    failure.recovery = RecoveryStatus::Unknown;
    failure.action =
        "Preserve the session and recover its independently decodable segments.".into();
    let publication = (|| {
        let index =
            index.ok_or_else(|| RecorderError::invalid("Missing restart session index."))?;
        let snapshot = index.snapshot()?;
        if snapshot.segments.iter().any(|segment| {
            segment.checkpoint.is_some() || segment.state == frame_mux::SegmentState::Closed
        }) {
            failure.recovery = RecoveryStatus::Recoverable;
        }
        let previous = snapshot
            .segments
            .iter()
            .rev()
            .find_map(|segment| segment.interruption_reason.as_deref())
            .unwrap_or("encoder segment restart interrupted");
        // Reserve room for the new error even if the previous native error was
        // already at the manifest's bound. Truncate only on UTF-8 boundaries.
        let mut end = previous.len().min(2600);
        while !previous.is_char_boundary(end) {
            end -= 1;
        }
        let mut reason = previous[..end].to_string();
        reason.push_str(&format!("; restart_failure_stage={stage}; failed_preparation_frames_discarded={preparation_frames_discarded}; restart_failure="));
        reason.extend(failure.message.chars().take(250));
        if reason.len() > 4096 {
            let mut end = 4096;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        index.record_interruption_reason(&reason)
    })();
    if let Err(error) = publication {
        failure.message.push_str(&format!(
            " Restart failure metadata could not be published: {}",
            error.message
        ));
    }
    failure
}

/// Preserve distinct discard categories; they overlap and must not be summed
/// into a fictitious count. Unaccepted owner surfaces are named separately.
pub(crate) fn interruption_reason(
    failure: &str,
    previous_id: &str,
    counters: EncoderCounterSnapshot,
    owner_pending: u64,
    owner_lookahead: u64,
    owner_submit_errors: u64,
    replacement: Option<(&str, Option<&str>, u64)>,
) -> String {
    // Keep accounting before free-form native text, so bounded later-stage
    // annotations cannot truncate the frame-loss categories.
    let mut end = failure.len().min(650);
    while !failure.is_char_boundary(end) {
        end -= 1;
    }
    let fault = &failure[..end];
    let mut reason = format!(
        "accepted_inputs={}; packets_dequeued={}; native_failures={}; inputs_discarded_on_failure={}; slots_abandoned_on_failure={}; packets_discarded_on_failure={}; packets_discarded_on_stop={}; owner_pending_discarded={owner_pending}; owner_lookahead_discarded={owner_lookahead}; owner_submit_errors={owner_submit_errors}; native_failure={fault}; failed_backend={previous_id}",
        counters.accepted_inputs,
        counters.packets_dequeued,
        counters.native_failures,
        counters.inputs_discarded_on_failure,
        counters.slots_abandoned_on_failure,
        counters.packets_discarded_on_failure,
        counters.packets_discarded_on_stop,
    );
    if let Some((id, declined, preparation_discarded)) = replacement {
        reason.push_str(&format!("; replacement_backend={id}"));
        reason.push_str(&format!(
            "; preparation_frames_discarded={preparation_discarded}"
        ));
        if let Some(declined) = declined {
            reason.push_str("; ");
            reason.extend(declined.chars().take(150));
        }
    }
    if reason.len() > 4096 {
        let mut end = 4096;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    reason
}

pub(crate) fn finish_index(
    index: &mut frame_mux::RecordingSession,
    artifact: RecordingArtifact,
) -> Result<RecordingArtifact, RecorderError> {
    let manifest = index.finish()?;
    if !manifest.completed {
        return Err(RecorderError {
            code: ErrorCode::FinalizationFailed,
            recovery: RecoveryStatus::Recoverable,
            message: format!(
                "The latest segment finalized at {}; the recording contains an interrupted segment. Session manifest: {}",
                artifact.path,
                index.manifest_path().display()
            ),
            action: "Recover the session segments; preserve every original file.".into(),
        });
    }
    Ok(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_audio::{TrackKind, TrackPcm, TrackPlan};
    use frame_clock::MediaTimestamp;
    use frame_contracts::AudioSourceId;
    use frame_encode::opus::OpusTrackEncoder;
    use frame_encode::{BitstreamFormat, EncoderCapability, VideoColour};
    use frame_mux::{AvAudioTrack, ContainerWriter, SegmentState};
    use std::num::{NonZeroU16, NonZeroU32};
    #[test]
    fn preparation_audio_failure_joins_owner_and_reports_cleanup_failure() {
        let (send, receive) = std::sync::mpsc::channel();
        let joined = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner_joined = joined.clone();
        let thread = std::thread::spawn(move || {
            receive.recv().unwrap();
            owner_joined.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(42)
        });
        let mut send = Some(send);
        let error = join_preparation(
            thread,
            std::time::Duration::from_secs(5),
            || {
                send.take().unwrap().send(()).unwrap();
                Err(RecorderError::invalid("injected audio service fault"))
            },
            |owner| {
                assert_eq!(*owner, 42);
                assert!(joined.load(std::sync::atomic::Ordering::SeqCst));
                Err(RecorderError::invalid("injected native cleanup fault"))
            },
        )
        .unwrap_err();
        assert!(error.message.starts_with("injected audio service fault"));
        assert!(error.message.contains("injected native cleanup fault"));
    }

    #[test]
    fn expired_preparation_rejects_joined_owner_without_detaching_it() {
        let joined = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner_joined = joined.clone();
        let thread = std::thread::spawn(move || {
            owner_joined.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(42)
        });
        let error = join_preparation(
            thread,
            std::time::Duration::ZERO,
            || Ok(()),
            |owner| {
                assert_eq!(*owner, 42);
                assert!(joined.load(std::sync::atomic::Ordering::SeqCst));
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::EncoderUnavailable);
        assert_eq!(error.recovery, RecoveryStatus::Unknown);
    }

    #[test]
    fn successful_preparation_retains_ownership_without_cleanup() {
        let thread = std::thread::spawn(|| Ok(42));
        let owner = join_preparation(
            thread,
            std::time::Duration::from_secs(5),
            || Ok(()),
            |_| panic!("Accepted preparation must transfer ownership, not clean it up."),
        )
        .unwrap();
        assert_eq!(owner, 42);
    }

    #[test]
    fn replacement_manifest_failure_preserves_native_context_and_prior_publication() {
        let root = std::env::temp_dir().join("freym-restart-metadata");
        std::fs::create_dir_all(&root).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = root.join(format!("{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = manifest_path(&directory.join("metadata.mkv"));
        let mut index = frame_mux::RecordingSession::create(&path).unwrap();
        let stream = fixture_stream();
        let origin = MediaTimestamp(7_000_000_000);
        let mut old = index.create_video_segment(&stream, origin).unwrap();
        old.write(video(origin)).unwrap();
        old.checkpoint().unwrap();
        index
            .record_interruption_reason("injected native fault; inputs_discarded_on_failure=1")
            .unwrap();
        drop(old);
        let next = index
            .create_video_segment(&stream, MediaTimestamp(origin.0 + 20_000_000))
            .unwrap();
        let fault = RecorderError::invalid("故障".repeat(3000));
        let result =
            record_restart_failure(Some(&mut index), "replacement_replay_contract", 1, fault);
        assert_eq!(result.recovery, RecoveryStatus::Recoverable);
        let snapshot = index.snapshot().unwrap();
        let reason = snapshot.segments[1].interruption_reason.as_deref().unwrap();
        assert!(reason.len() <= 4096);
        assert!(reason.contains("inputs_discarded_on_failure=1"));
        assert!(reason.contains("restart_failure_stage=replacement_replay_contract"));
        assert!(reason.contains("failed_preparation_frames_discarded=1"));
        let before = std::fs::read(&path).unwrap();
        let mut blocked = path.as_os_str().to_os_string();
        blocked.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            snapshot.generation + 1
        ));
        std::fs::create_dir(PathBuf::from(blocked)).unwrap();
        let result = record_restart_failure(
            Some(&mut index),
            "replacement_manifest",
            1,
            RecorderError::invalid("injected publication fault"),
        );
        assert!(result.message.contains("injected publication fault"));
        assert!(result.message.contains("could not be published"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(index.snapshot().is_err()); // Publication failure is sticky.
        assert_eq!(frame_mux::read_session_manifest(&path).unwrap(), snapshot);
        drop(next);
        println!(
            "replacement metadata publication fixture: {}",
            directory.display()
        );
    }

    #[test]
    fn interruption_metadata_names_disjoint_discard_categories_and_replacement() {
        let counters = EncoderCounterSnapshot {
            accepted_inputs: 8,
            packets_dequeued: 4,
            native_failures: 1,
            slots_abandoned_on_failure: 2,
            inputs_discarded_on_failure: 1,
            packets_discarded_on_failure: 1,
            packets_discarded_on_stop: 1,
            ..EncoderCounterSnapshot::default()
        };
        let reason = interruption_reason(
            "native collect fault",
            "old-backend",
            counters,
            1,
            1,
            1,
            Some(("new-qualified-backend", Some("qualification detail"), 1)),
        );
        for field in [
            "accepted_inputs=8",
            "slots_abandoned_on_failure=2",
            "owner_pending_discarded=1",
            "owner_lookahead_discarded=1",
            "owner_submit_errors=1",
            "preparation_frames_discarded=1",
            "replacement_backend=new-qualified-backend",
        ] {
            assert!(reason.contains(field), "{reason}");
        }
        let bounded = interruption_reason(
            &"故障".repeat(4096),
            &"x".repeat(1024),
            counters,
            0,
            0,
            0,
            Some((&"y".repeat(1024), Some(&"故障".repeat(4096)), 0)),
        );
        assert!(bounded.len() <= 4096);
        for field in [
            "accepted_inputs=8",
            "slots_abandoned_on_failure=2",
            "owner_pending_discarded=0",
            "owner_lookahead_discarded=0",
            "owner_submit_errors=0",
        ] {
            assert!(bounded.contains(field), "{bounded}");
        }
    }
    #[test]
    fn only_exact_encoder_failure_in_live_phase_gets_one_attempt() {
        let error = RecorderError::invalid("native encode failed");
        let mut budget = RestartBudget {
            enabled: true,
            alternative_backend: true,
            attempts: 0,
        };
        assert!(!budget.begin(false, &error, Some(error.clone())));
        assert!(!budget.begin(true, &error, Some(RecorderError::invalid("disk failed"))));
        assert!(budget.begin(true, &error, Some(error.clone())));
        assert!(!budget.begin(true, &error, Some(error.clone())));
        assert!(!RestartBudget::default().begin(true, &error, Some(error.clone())));
    }

    #[test]
    fn a_new_join_failure_or_any_retained_native_work_forbids_reuse() {
        let fault = RecorderError::invalid("injected encode failure");
        let stopped = Err(fault.clone());
        let safe = EncoderCounterSnapshot {
            // Accounted discarded packets/inputs are allowed in a failed segment.
            accepted_inputs: 3,
            native_submissions: 2,
            packets_discarded_on_failure: 2,
            inputs_discarded_on_failure: 1,
            ..EncoderCounterSnapshot::default()
        };
        validate_cleanup(&fault, &stopped, safe).unwrap();
        let panic = Err(RecorderError::invalid("worker join panicked"));
        assert!(validate_cleanup(&fault, &panic, safe).is_err());
        let unsafe_snapshots = [
            EncoderCounterSnapshot {
                accepted_inputs: 4,
                ..safe
            },
            EncoderCounterSnapshot {
                retained_native_owner: true,
                ..safe
            },
            EncoderCounterSnapshot {
                retained_leases: 1,
                ..safe
            },
            EncoderCounterSnapshot {
                leases_kept_alive: 1,
                ..safe
            },
            EncoderCounterSnapshot {
                slots_kept_alive_on_failure: 1,
                ..safe
            },
            EncoderCounterSnapshot {
                unsafe_cleanup_steps: 1,
                ..safe
            },
            EncoderCounterSnapshot {
                queued_inputs: 1,
                ..safe
            },
            EncoderCounterSnapshot {
                inflight: 1,
                ..safe
            },
        ];
        for counters in unsafe_snapshots {
            assert!(validate_cleanup(&fault, &stopped, counters).is_err());
        }
    }

    fn fixture_stream() -> EncodedStreamInfo {
        EncodedStreamInfo {
            capability: EncoderCapability {
                id: "injected-worker-hevc".into(),
                codec: "hevc".into(),
                hardware: false,
            },
            width: NonZeroU32::new(64).unwrap(),
            height: NonZeroU32::new(64).unwrap(),
            nominal_rate_num: NonZeroU32::new(50).unwrap(),
            nominal_rate_den: NonZeroU32::new(1).unwrap(),
            bitstream_format: BitstreamFormat::HevcAnnexB,
            colour: Some(VideoColour::BT2020_PQ_LIMITED),
            codec_config: include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.hevc")
                .to_vec(),
            allows_reordering: false,
        }
    }

    fn video(origin: MediaTimestamp) -> EncodedPacket {
        EncodedPacket {
            track_id: 0,
            pts: origin,
            dts: origin,
            duration_ns: 20_000_000,
            keyframe: true,
            data: include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.hevc").to_vec(),
        }
    }

    // Actual fresh libopus instances feed the actual segment writer. This fixture
    // injects a published worker outcome, not a native driver or capture surface.
    fn opus_segment(origin: MediaTimestamp) -> (Vec<AvAudioTrack>, Vec<EncodedPacket>, u16) {
        let mut encoder = OpusTrackEncoder::new(0, origin).unwrap();
        let plan = TrackPlan {
            kind: TrackKind::MainMix,
            sources: vec![AudioSourceId("fixture-source".into())],
        };
        let tracks = vec![AvAudioTrack::from_plan(&plan, encoder.pre_skip()).unwrap()];
        let mut packets = Vec::new();
        for offset in [0, 10_000_000] {
            let pcm = TrackPcm {
                track_index: 0,
                timestamp: MediaTimestamp(origin.0 + offset),
                sample_rate: NonZeroU32::new(48_000).unwrap(),
                channels: NonZeroU16::new(2).unwrap(),
                samples: vec![0.1; 960],
            };
            if let Some(packet) = encoder.submit(&pcm).unwrap() {
                packets.push(packet);
            }
        }
        let finish = encoder.finish().unwrap();
        assert_eq!(finish.input_frames, 960);
        packets.extend(finish.packets);
        (tracks, packets, finish.discard_padding)
    }

    #[test]
    fn injected_worker_failure_uses_new_av_tracks_and_keeps_session_recoverable() {
        let root = std::env::temp_dir().join("freym-restart-fixtures");
        std::fs::create_dir_all(&root).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = root.join(format!("{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let output = directory.join("injected.mkv");
        let path = manifest_path(&output);
        let mut index = frame_mux::RecordingSession::create(&path).unwrap();
        let stream = fixture_stream();
        let old_origin = frame_clock::MediaTimestamp(7_000_000_000);
        let (old_tracks, old_packets, old_trim) = opus_segment(old_origin);
        let mut old = index
            .create_av_segment(&stream, &old_tracks, old_origin, true)
            .unwrap();
        old.write_video(video(old_origin)).unwrap();
        for packet in old_packets {
            old.write_audio(packet).unwrap();
        }
        old.finish_audio(0, old_trim).unwrap();
        old.checkpoint().unwrap();
        let fault = RecorderError::invalid("injected published worker failure");
        let mut budget = RestartBudget {
            enabled: true,
            alternative_backend: true,
            attempts: 0,
        };
        assert!(budget.begin(true, &fault, Some(fault.clone())));
        index.record_interruption_reason(&fault.message).unwrap();
        validate_cleanup(
            &fault,
            &Err(fault.clone()),
            EncoderCounterSnapshot::default(),
        )
        .unwrap();
        drop(old); // Deliberately never finish the failed video stream.
        index
            .record_interruption_reason("injected fault; post-cleanup accounting=explicit")
            .unwrap();
        let preparation_failure = record_restart_failure(
            Some(&mut index),
            "replacement_preparation",
            1,
            RecorderError::invalid("injected replacement unavailable"),
        );
        assert_eq!(preparation_failure.recovery, RecoveryStatus::Recoverable);
        assert!(
            preparation_failure
                .message
                .contains("injected replacement unavailable")
        );
        let before = index.snapshot().unwrap();
        assert_eq!(before.segments[0].state, SegmentState::Interrupted);
        let original = std::fs::read(&output).unwrap();

        let new_origin = frame_clock::MediaTimestamp(old_origin.0 + 20_000_000);
        let (tracks, packets, trim) = opus_segment(new_origin);
        let mut next = index
            .create_av_segment(&stream, &tracks, new_origin, true)
            .unwrap();
        let mut dependent = video(new_origin);
        dependent.data = vec![0, 0, 0, 1, 0x02, 0x01, 0x80]; // HEVC TRAIL; lying keyframe flag.
        assert!(validate_first_packet(&stream, &dependent).is_err());
        assert_eq!(next.stats().video_packets, 0);
        let mut falsely_unflagged = video(new_origin);
        falsely_unflagged.keyframe = false;
        assert!(validate_first_packet(&stream, &falsely_unflagged).is_err());
        let first = video(new_origin);
        validate_first_packet(&stream, &first).unwrap();
        next.write_video(first).unwrap();
        for packet in packets {
            assert!(packet.pts >= new_origin);
            next.write_audio(packet).unwrap();
        }
        next.finish_audio(0, trim).unwrap();
        next.finish_video(20_000_000).unwrap();
        let artifact = next.finalize().unwrap();
        let latest = std::fs::read(&artifact.path).unwrap();
        let error = finish_index(&mut index, artifact).unwrap_err();
        assert_eq!(error.recovery, RecoveryStatus::Recoverable);
        let manifest = index.snapshot().unwrap();
        assert!(!manifest.completed);
        assert_eq!(manifest.segments.len(), 2);
        assert_eq!(manifest.segments[1].state, SegmentState::Closed);
        assert_ne!(
            manifest.segments[0].file_name,
            manifest.segments[1].file_name
        );
        assert_eq!(
            manifest.segments[0].tracks_ebml,
            manifest.segments[1].tracks_ebml
        );
        assert_eq!(manifest.segments[1].origin_ns, new_origin.0);
        let interruption = manifest.segments[0].interruption_reason.as_deref().unwrap();
        assert!(interruption.contains("post-cleanup accounting=explicit"));
        assert!(interruption.contains("restart_failure_stage=replacement_preparation"));
        assert!(interruption.contains("failed_preparation_frames_discarded=1"));
        assert!(interruption.contains("injected replacement unavailable"));
        assert_eq!(std::fs::read(&output).unwrap(), original);
        assert!(!budget.begin(true, &fault, Some(fault.clone())));
        drop(index);
        let recovered =
            frame_mux::recover_session(&path, &directory.join("recovered.freym-session")).unwrap();
        assert_eq!(recovered.verified_closed_segments, 1);
        assert_eq!(recovered.recovered_tails.len(), 1);
        assert_eq!(
            std::fs::read(directory.join(&recovered.manifest.segments[1].file_name)).unwrap(),
            latest
        );
        assert_eq!(std::fs::read(&output).unwrap(), original);
        assert!(!recovered.manifest.completed);
        println!(
            "injected segmented A/V worker failure fixture: {}",
            directory.display()
        );
    }
}
