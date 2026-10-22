//! Bounded disk work. No native capture objects leave their owner thread.
use frame_contracts::{RecorderError, ReplaySaveResult};
use frame_mux::replay::{DiskReplayLimit, ReplayBuffer, RollingReplayStore};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, SyncSender, TrySendError},
};
use std::thread::JoinHandle;
use std::time::Duration;

struct Save {
    buffer: ReplayBuffer,
    path: PathBuf,
    seconds: u32,
    metadata: Option<ReplayMetadata>,
}
pub(crate) enum ReplayMetadata {
    #[cfg(target_os = "windows")]
    File(PathBuf),
    Rolling(frame_metadata::ReplayMetadataSnapshot),
}

type CheckpointSnapshot = (ReplayBuffer, Option<frame_metadata::RollingMetadata>);

pub(crate) struct ReplayWorker {
    saves: Option<SyncSender<Save>>,
    // Coalesce checkpoints instead of accumulating disk work behind an export.
    checkpoint: Arc<Mutex<Option<CheckpointSnapshot>>>,
    results: Receiver<Result<ReplaySaveResult, RecorderError>>,
    archive_error: Arc<Mutex<Option<RecorderError>>>,
    join: Option<JoinHandle<()>>,
    last_checkpoint_end: u64,
    segment_ns: u64,
    metadata: Option<frame_metadata::RollingMetadata>,
}

impl ReplayWorker {
    pub(crate) fn start(directory: PathBuf, limit: DiskReplayLimit) -> Result<Self, RecorderError> {
        Self::start_impl(
            directory,
            limit,
            #[cfg(test)]
            None,
        )
    }

    fn start_impl(
        directory: PathBuf,
        limit: DiskReplayLimit,
        #[cfg(test)] mut stall: Option<(SyncSender<()>, Receiver<()>)>,
    ) -> Result<Self, RecorderError> {
        let (saves, jobs) = mpsc::sync_channel::<Save>(2);
        let (results, received) = mpsc::sync_channel(4);
        let checkpoint = Arc::new(Mutex::new(None::<CheckpointSnapshot>));
        let pending = checkpoint.clone();
        let archive_error = Arc::new(Mutex::new(None));
        let failure = archive_error.clone();
        let join = std::thread::Builder::new()
            .name("freym-replay-io".into())
            .spawn(move || {
                let mut store = match RollingReplayStore::create(&directory, limit) {
                    Ok(store) => Some(store),
                    Err(error) => {
                        *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                        None
                    }
                };
                loop {
                    let disconnected = match jobs.recv_timeout(Duration::from_millis(20)) {
                        Ok(job) => {
                            #[cfg(test)]
                            if let Some((entered, release)) = stall.take() {
                                let _ = entered.send(());
                                let _ = release.recv_timeout(Duration::from_secs(5));
                            }
                            let bytes = job.buffer.retained_bytes() as u32;
                            let duration = u64::from(job.seconds) * 1_000_000_000;
                            let result = match &job.metadata {
                                Some(ReplayMetadata::Rolling(events)) => {
                                    job.buffer.save_session_with_rolling_metadata(
                                        &job.path, duration, events,
                                    )
                                }
                                other => job.buffer.save_session_with_metadata(
                                    &job.path,
                                    duration,
                                    match other {
                                        #[cfg(target_os = "windows")]
                                        Some(ReplayMetadata::File(p)) => Some(p.as_path()),
                                        _ => None,
                                    },
                                ),
                            }
                            .map(|artifact| ReplaySaveResult {
                                artifact,
                                requested_seconds: job.seconds,
                                retained_bytes: bytes,
                            });
                            // Results are bounded independently from packet snapshots.
                            let _ = results.send(result);
                            false
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => false,
                        Err(mpsc::RecvTimeoutError::Disconnected) => true,
                    };
                    let snapshot = pending.lock().unwrap_or_else(|p| p.into_inner()).take();
                    if let (Some((buffer, metadata)), Some(archive)) = (snapshot, store.as_mut())
                        && let Err(error) = match metadata {
                            Some(events) => archive.checkpoint_with_metadata(&buffer, &events),
                            None => archive.checkpoint_if_due(&buffer),
                        }
                    {
                        *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                        store = None; // preserve the failed archive; no further deletion
                    }
                    if disconnected {
                        break;
                    }
                }
            })
            .map_err(|e| {
                RecorderError::invalid(format!("Could not start replay disk worker: {e}"))
            })?;
        Ok(Self {
            saves: Some(saves),
            checkpoint,
            results: received,
            archive_error,
            join: Some(join),
            last_checkpoint_end: 0,
            segment_ns: limit.segment_ns,
            metadata: None,
        })
    }

    pub(crate) fn checkpoint(
        &mut self,
        buffer: &ReplayBuffer,
        metadata: Option<frame_metadata::RollingMetadata>,
    ) {
        if metadata.is_some() {
            self.metadata = metadata;
        }
        let end = buffer.latest_end_ns();
        if end < self.last_checkpoint_end.saturating_add(self.segment_ns) {
            return;
        }
        if let Ok(mut slot) = self.checkpoint.try_lock() {
            *slot = Some((buffer.clone(), self.metadata.clone()));
            self.last_checkpoint_end = end;
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn checkpoint_final(&mut self, buffer: &ReplayBuffer) {
        *self.checkpoint.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((buffer.clone(), self.metadata.clone()));
    }

    #[cfg(test)]
    pub(crate) fn save(
        &self,
        buffer: ReplayBuffer,
        path: PathBuf,
        seconds: u32,
    ) -> Result<(), RecorderError> {
        self.save_with_metadata(buffer, path, seconds, None)
    }
    pub(crate) fn save_with_metadata(
        &self,
        buffer: ReplayBuffer,
        path: PathBuf,
        seconds: u32,
        metadata: Option<ReplayMetadata>,
    ) -> Result<(), RecorderError> {
        if self.join.as_ref().is_none_or(|join| join.is_finished()) {
            return Err(RecorderError::invalid(
                "Replay disk worker stopped; preserve recovery files and restart replay.",
            ));
        }
        self.saves
            .as_ref()
            .expect("live worker")
            .try_send(Save {
                buffer,
                path,
                seconds,
                metadata,
            })
            .map_err(|e| {
                RecorderError::invalid(match e {
                    TrySendError::Full(_) => {
                        "Replay export is busy; wait for a saved clip before saving again."
                    }
                    TrySendError::Disconnected(_) => "Replay disk worker stopped; restart replay.",
                })
            })
    }

    pub(crate) fn results(&self) -> Vec<Result<ReplaySaveResult, RecorderError>> {
        let mut results: Vec<_> = self.results.try_iter().collect();
        if let Some(error) = self
            .archive_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            results.push(Err(error));
        }
        results
    }

    pub(crate) fn shutdown(&mut self) -> Vec<Result<ReplaySaveResult, RecorderError>> {
        self.saves.take();
        let mut results = self.results();
        if let Some(join) = self.join.take() {
            while !join.is_finished() {
                if let Ok(result) = self.results.recv_timeout(Duration::from_millis(20)) {
                    results.push(result);
                }
            }
            if join.join().is_err() {
                results.push(Err(RecorderError::invalid(
                    "Replay disk worker panicked; preserve its recovery files.",
                )));
            }
        }
        results.extend(self.results());
        results
    }
}

impl Drop for ReplayWorker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_clock::MediaTimestamp;
    use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
    use frame_mux::replay::ReplayLimit;
    use std::num::NonZeroU32;

    #[test]
    fn stalled_export_keeps_capture_bounded_and_snapshots_playable_after_eviction() {
        let directory = std::env::temp_dir()
            .join("freym-replay-worker")
            .join(format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&directory).unwrap();
        let payload =
            include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.hevc").to_vec();
        let mut buffer = ReplayBuffer::new(
            EncodedStreamInfo {
                capability: EncoderCapability {
                    id: "hevc-fixture".into(),
                    codec: "hevc".into(),
                    hardware: false,
                },
                width: NonZeroU32::new(64).unwrap(),
                height: NonZeroU32::new(64).unwrap(),
                nominal_rate_num: NonZeroU32::new(10).unwrap(),
                nominal_rate_den: NonZeroU32::new(1).unwrap(),
                bitstream_format: BitstreamFormat::HevcAnnexB,
                colour: Some(frame_encode::VideoColour::BT2020_PQ_LIMITED),
                codec_config: payload.clone(),
                allows_reordering: false,
            },
            vec![],
            ReplayLimit {
                duration_ns: 2_000_000_000,
                max_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let push = |buffer: &mut ReplayBuffer, second: u64| {
            for index in 0..10 {
                let pts = second * 1_000_000_000 + index * 100_000_000;
                buffer.push_video(&EncodedPacket {
                    track_id: 0,
                    pts: MediaTimestamp(pts),
                    dts: MediaTimestamp(pts),
                    duration_ns: 100_000_000,
                    keyframe: true,
                    data: payload.clone(),
                });
            }
        };
        push(&mut buffer, 0);
        let mut recorder = frame_metadata::MetadataRecorder::start_rolling(
            Arc::new(frame_clock::MasterClock::default()),
            frame_clock::PauseTimeline::default(),
            frame_metadata::Header {
                version: 1,
                origin_ns: 0,
                cursor_baked: Some(false),
                capabilities: frame_metadata::Capabilities::default(),
            },
            2_000_000_000,
        )
        .unwrap();
        let event = |time: u64, kind| {
            recorder
                .boundary(MediaTimestamp(time), MediaTimestamp(time), kind)
                .unwrap()
        };
        event(
            0,
            frame_metadata::EventKind::Geometry {
                bounds: frame_metadata::Geometry {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
            },
        );
        event(0, frame_metadata::EventKind::Cursor { x: 10, y: 10 });
        event(
            100_000_000,
            frame_metadata::EventKind::Button {
                x: 10,
                y: 10,
                button: frame_metadata::Button::Left,
                pressed: true,
            },
        );
        let metadata = recorder.rolling().unwrap();
        let (entered, wait) = mpsc::sync_channel(1);
        let (release, stalled) = mpsc::sync_channel(1);
        let mut worker = ReplayWorker::start_impl(
            directory.join("owned"),
            DiskReplayLimit {
                segment_ns: 1_000_000_000,
                history_ns: 2_000_000_000,
                max_bytes: 16 * 1024 * 1024,
                max_segments: 2,
            },
            Some((entered, stalled)),
        )
        .unwrap();
        worker
            .save_with_metadata(
                buffer.clone(),
                directory.join("first.mkv"),
                1,
                Some(ReplayMetadata::Rolling(
                    metadata.pin(0, 1_000_000_000).unwrap(),
                )),
            )
            .unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        worker
            .save(buffer.clone(), directory.join("second.mkv"), 1)
            .unwrap();
        worker
            .save(buffer.clone(), directory.join("third.mkv"), 1)
            .unwrap();
        assert!(
            worker
                .save(buffer.clone(), directory.join("busy.mkv"), 1)
                .is_err()
        );
        for second in 1..8 {
            event(
                second * 1_000_000_000,
                frame_metadata::EventKind::Cursor {
                    x: second as u32,
                    y: 10,
                },
            );
            event(
                second * 1_000_000_000 + 100_000_000,
                frame_metadata::EventKind::Scroll {
                    x: second as u32,
                    y: 10,
                    horizontal: false,
                    delta: 120,
                },
            );
            push(&mut buffer, second);
            worker.checkpoint(&buffer, Some(metadata.clone()));
            assert!(buffer.retained_bytes() <= 1024 * 1024);
        }
        assert_eq!(buffer.latest_end_ns(), 8_000_000_000);
        release.send(()).unwrap();
        let results = worker.shutdown();
        assert_eq!(results.len(), 3, "{results:?}");
        for result in results {
            let result = result.unwrap();
            assert!((result.artifact.duration_ms - 1000.0).abs() < 1.0);
            let decoded = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-xerror", "-i"])
                .arg(&result.artifact.path)
                .args(["-f", "null", "-"])
                .output()
                .unwrap();
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
        }
        assert!(!directory.join("busy.mkv").exists());
        let read_events = |manifest: &PathBuf| {
            let index = frame_mux::read_session_manifest(manifest).unwrap();
            let mut events = Vec::new();
            frame_metadata::read_sidecar(
                &manifest
                    .parent()
                    .unwrap()
                    .join(index.metadata_file.unwrap()),
                |record| {
                    if let frame_metadata::Record::Event(e) = record {
                        events.push(e.clone());
                    }
                    Ok(())
                },
            )
            .unwrap();
            events
        };
        let saved = read_events(&directory.join("first.mkv.freym-session"));
        assert!(saved.iter().any(|e| matches!(
            e.event,
            frame_metadata::EventKind::Button { pressed: true, .. }
        )));
        assert!(saved.iter().all(|e| e.session_ns < 1_000_000_000));
        let archived = read_events(&directory.join("owned/replay.freym-session"));
        assert!(
            archived
                .iter()
                .all(|e| e.session_ns >= 7_000_000_000 && e.session_ns < 8_000_000_000)
        );
        assert_eq!(
            archived
                .iter()
                .filter(|e| matches!(e.event, frame_metadata::EventKind::Scroll { .. }))
                .count(),
            1
        );
        // A second seal exercises retirement with an associated event sidecar.
        event(
            8_000_000_000,
            frame_metadata::EventKind::Cursor { x: 8, y: 10 },
        );
        event(
            8_100_000_000,
            frame_metadata::EventKind::Scroll {
                x: 8,
                y: 10,
                horizontal: false,
                delta: 120,
            },
        );
        push(&mut buffer, 8);
        let mut archive = RollingReplayStore::create(
            &directory.join("retirement"),
            DiskReplayLimit {
                segment_ns: 1_000_000_000,
                history_ns: 2_000_000_000,
                max_bytes: 16 * 1024 * 1024,
                max_segments: 1,
            },
        )
        .unwrap();
        archive
            .checkpoint_with_metadata(&buffer, &metadata)
            .unwrap();
        event(
            9_000_000_000,
            frame_metadata::EventKind::Cursor { x: 9, y: 10 },
        );
        event(
            9_100_000_000,
            frame_metadata::EventKind::Scroll {
                x: 9,
                y: 10,
                horizontal: false,
                delta: 120,
            },
        );
        push(&mut buffer, 9);
        archive
            .checkpoint_with_metadata(&buffer, &metadata)
            .unwrap();
        let retained = read_events(&directory.join("retirement/replay.freym-session"));
        assert_eq!(archive.stats().retired_segments, 1);
        assert!(retained.iter().all(|e| e.session_ns >= 9_000_000_000));
        drop(archive);
        // A crash between sidecar write and publication must not hide sealed media
        // or delete evidence. The previous published events remain usable.
        let pending = directory.join("retirement/events.pending");
        std::fs::write(&pending, b"interrupted metadata replacement").unwrap();
        let recovered = RollingReplayStore::recover_owned(
            &directory.join("retirement"),
            &directory.join("retirement-recovered.freym-session"),
        )
        .unwrap();
        assert_eq!(recovered.verified_closed_segments, 1);
        assert!(recovered.rejected_segments.is_empty());
        assert_eq!(
            std::fs::read(pending).unwrap(),
            b"interrupted metadata replacement"
        );
        assert_eq!(
            read_events(&directory.join("retirement-recovered.freym-session")),
            retained
        );
        // Stopping the real observer drains the window; the handle remains usable.
        let _ = recorder.finish(false);
        let before = std::fs::read(directory.join("first.mkv")).unwrap();
        let mut worker = ReplayWorker::start(
            directory.join("new-owned"),
            DiskReplayLimit {
                segment_ns: 1_000_000_000,
                history_ns: 2_000_000_000,
                max_bytes: 16 * 1024 * 1024,
                max_segments: 2,
            },
        )
        .unwrap();
        worker
            .save(buffer.clone(), directory.join("first.mkv"), 1)
            .unwrap();
        assert!(worker.shutdown().into_iter().all(|result| result.is_err()));
        assert_eq!(std::fs::read(directory.join("first.mkv")).unwrap(), before);
    }
}
