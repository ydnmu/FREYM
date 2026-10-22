//! The byte target both Matroska writers stream into.
//!
//! [`OutputTarget`] keeps the backend's own sink type out of this crate's public
//! surface and states the two properties the writers depend on: a position, and a
//! `finish` that only succeeds once every buffer reached the operating system.

use crate::journal::RecordingJournal;
use mkwriter::sink::{SeekSink, Sink};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

/// What a writer needs from its byte target.
pub(super) trait OutputTarget: Write {
    fn position(&self) -> u64;
    fn seekable(&self) -> bool;
    fn patch(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Flush buffered media and request durable file synchronization. This does
    /// not finalize the recording or establish a hardware power-loss guarantee.
    fn checkpoint(&mut self) -> io::Result<()>;
    fn segment_headers(&mut self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn segment_checkpoint(&mut self, _: super::DurableCheckpoint) -> io::Result<()> {
        Ok(())
    }
    /// Flush a complete prefix and start its durable synchronization. Targets
    /// without a separate durability owner retain synchronous semantics.
    fn request_checkpoint(
        &mut self,
        point: super::DurableCheckpoint,
    ) -> io::Result<Option<super::DurableCheckpoint>> {
        self.checkpoint()?;
        self.segment_checkpoint(point)?;
        Ok(Some(point))
    }
    /// Only a completed file sync AND manifest publication acknowledge a point.
    /// `wait` is reserved for explicit lifecycle barriers, never live packets.
    fn poll_checkpoint(&mut self, _: bool) -> io::Result<Option<super::DurableCheckpoint>> {
        Ok(None)
    }
    fn segment_finalization(&mut self, _: u64, _: u64) {}
    /// Flushes every buffer, hands the bytes to the operating system and closes the
    /// file. Only a successful finish may produce an artifact.
    fn finish(self: Box<Self>) -> io::Result<()>;
}

/// A `create_new` file streamed through a seekable buffered sink.
pub(super) struct OutputFile {
    sink: SeekSink<BufWriter<File>>,
    // SeekSink does not expose its file by reference. The duplicate is used only
    // for synchronization, never for reads, writes or seeking the shared cursor.
    sync_handle: Option<File>,
    durability: Option<DurabilityWorker>,
    journal: Option<RecordingJournal>,
    segment: Option<(std::path::PathBuf, crate::session::SegmentLease)>,
    finalization: Option<(u64, u64)>,
}

struct DurabilityResources {
    file: File,
    segment: Option<(std::path::PathBuf, crate::session::SegmentLease)>,
}

/// One bounded metadata request, no media buffers. The existing cloned file
/// handle only syncs; all writes, seeks and frame order stay on the mux owner.
/// The worker sleeps until the first/periodic checkpoint or a lifecycle barrier.
struct DurabilityWorker {
    requests: Option<SyncSender<super::DurableCheckpoint>>,
    results: Receiver<io::Result<super::DurableCheckpoint>>,
    join: Option<JoinHandle<DurabilityResources>>,
    pending: bool,
    failure: Option<(io::ErrorKind, String)>,
}

impl DurabilityWorker {
    fn spawn(resources: DurabilityResources) -> io::Result<Self> {
        Self::spawn_with(resources, |resources, point| {
            resources.file.sync_all()?;
            if let Some((_, lease)) = &mut resources.segment {
                lease.checkpoint(point)?;
            }
            Ok(())
        })
    }

    fn spawn_with(
        mut resources: DurabilityResources,
        mut sync: impl FnMut(&mut DurabilityResources, super::DurableCheckpoint) -> io::Result<()>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (completed, results) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("freym-durability".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                while let Ok(point) = incoming.recv() {
                    let result = sync(&mut resources, point).map(|()| point);
                    if completed.send(result).is_err() {
                        break;
                    }
                }
                resources
            })?;
        Ok(Self {
            requests: Some(requests),
            results,
            join: Some(join),
            pending: false,
            failure: None,
        })
    }

    fn request(&mut self, point: super::DurableCheckpoint) -> io::Result<()> {
        if let Some((kind, message)) = &self.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        if self.pending {
            return Err(io::Error::other(
                "a durability checkpoint is already pending",
            ));
        }
        self.requests
            .as_ref()
            .expect("live durability owner")
            .try_send(point)
            .map_err(|error| io::Error::other(format!("durability request failed: {error}")))?;
        self.pending = true;
        Ok(())
    }

    fn poll(&mut self, wait: bool) -> io::Result<Option<super::DurableCheckpoint>> {
        if let Some((kind, message)) = &self.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        if !self.pending {
            return Ok(None);
        }
        let result = if wait {
            self.results.recv().unwrap_or_else(|_| {
                Err(io::Error::other(
                    "durability owner stopped without acknowledging its checkpoint",
                ))
            })
        } else {
            match self.results.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => Err(io::Error::other(
                    "durability owner stopped without acknowledging its checkpoint",
                )),
            }
        };
        self.pending = false;
        match result {
            Ok(point) => Ok(Some(point)),
            Err(error) => {
                self.failure = Some((error.kind(), error.to_string()));
                Err(error)
            }
        }
    }

    fn stop(mut self) -> io::Result<DurabilityResources> {
        // Even failed checkpoints join before a lease can finalize or interrupt.
        let result = self.poll(true);
        self.requests.take();
        let resources = self
            .join
            .take()
            .expect("durability thread")
            .join()
            .map_err(|_| io::Error::other("durability owner panicked"))?;
        result?;
        Ok(resources)
    }
}

impl Drop for DurabilityWorker {
    fn drop(&mut self) {
        self.requests.take();
        // Only one request can be in flight, so its single result slot cannot
        // block shutdown, even when an aborted writer never polls it.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl OutputFile {
    pub(super) fn new(file: File) -> io::Result<Self> {
        let sync_handle = file.try_clone()?;
        Ok(Self {
            sink: SeekSink::new(BufWriter::new(file))?,
            sync_handle: Some(sync_handle),
            durability: None,
            journal: None,
            segment: None,
            finalization: None,
        })
    }

    pub(super) fn recording(file: File, path: &Path) -> io::Result<Self> {
        let journal = RecordingJournal::create(path)?;
        let mut output = Self::new(file)?;
        output.journal = Some(journal);
        Ok(output)
    }

    fn stop_durability(&mut self) -> io::Result<()> {
        if let Some(worker) = self.durability.take() {
            let resources = worker.stop()?;
            self.sync_handle = Some(resources.file);
            self.segment = resources.segment;
        }
        Ok(())
    }

    pub(super) fn segment(
        file: File,
        path: &Path,
        lease: crate::session::SegmentLease,
    ) -> io::Result<Self> {
        let mut output = Self::recording(file, path)?;
        output.segment = Some((path.to_path_buf(), lease));
        Ok(output)
    }
}

impl Write for OutputFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

impl OutputTarget for OutputFile {
    fn position(&self) -> u64 {
        self.sink.position()
    }
    fn seekable(&self) -> bool {
        true
    }
    fn patch(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.sink.patch(offset, buf)
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        self.stop_durability()?;
        self.sink.flush()?;
        self.sync_handle
            .as_ref()
            .expect("owned sync handle")
            .sync_all()
    }
    fn request_checkpoint(
        &mut self,
        point: super::DurableCheckpoint,
    ) -> io::Result<Option<super::DurableCheckpoint>> {
        self.sink.flush()?;
        if self.durability.is_none() {
            self.durability = Some(DurabilityWorker::spawn(DurabilityResources {
                file: self.sync_handle.take().expect("owned sync handle"),
                segment: self.segment.take(),
            })?);
        }
        self.durability
            .as_mut()
            .expect("durability owner")
            .request(point)?;
        Ok(None)
    }
    fn poll_checkpoint(&mut self, wait: bool) -> io::Result<Option<super::DurableCheckpoint>> {
        self.durability
            .as_mut()
            .map_or(Ok(None), |worker| worker.poll(wait))
    }
    fn finish(mut self: Box<Self>) -> io::Result<()> {
        self.stop_durability()?;
        let OutputFile {
            sink,
            sync_handle,
            durability: _,
            journal,
            segment,
            finalization,
        } = *self;
        let file = sink
            .into_inner()?
            .into_inner()
            .map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        drop(sync_handle);
        if let Some((path, mut lease)) = segment {
            if let Some((end, cues)) = finalization {
                lease.finalization(end, cues);
            }
            lease.complete(&path)?;
        }
        if let Some(journal) = journal {
            journal.complete()?;
        }
        Ok(())
    }

    fn segment_headers(&mut self, headers: &[u8]) -> io::Result<()> {
        if let Some((_, lease)) = &mut self.segment {
            lease.headers(headers)?;
        }
        Ok(())
    }
    fn segment_checkpoint(&mut self, point: super::DurableCheckpoint) -> io::Result<()> {
        self.stop_durability()?;
        if let Some((_, lease)) = &mut self.segment {
            lease.checkpoint(point)?;
        }
        Ok(())
    }
    fn segment_finalization(&mut self, end: u64, cues: u64) {
        // Keep the facts on the output until finish joins the durability owner
        // and returns the same lease. No clone can prematurely interrupt it.
        self.finalization = Some((end, cues));
    }
}

/// Adapts a boxed target to the backend's sink trait.
pub(super) struct SinkAdapter(pub(super) Box<dyn OutputTarget>);

impl Write for SinkAdapter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Sink for SinkAdapter {
    fn position(&self) -> u64 {
        self.0.position()
    }
    fn seekable(&self) -> bool {
        self.0.seekable()
    }
    fn patch(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.0.patch(offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::Backend;
    use super::*;
    use mkwriter::{Frame, TrackEntry, Tracks, VideoSettings, ids};
    use std::fs::OpenOptions;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn file() -> (std::path::PathBuf, OutputFile) {
        let path = std::env::temp_dir().join(format!(
            "freym-durability-{}-{}.mkv",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        (path, OutputFile::new(file).unwrap())
    }

    fn inject_worker(
        output: &mut OutputFile,
        sync: impl FnMut(&mut DurabilityResources, super::super::DurableCheckpoint) -> io::Result<()>
        + Send
        + 'static,
    ) {
        output.durability = Some(
            DurabilityWorker::spawn_with(
                DurabilityResources {
                    file: output.sync_handle.take().unwrap(),
                    segment: output.segment.take(),
                },
                sync,
            )
            .unwrap(),
        );
    }

    fn backend(output: OutputFile) -> Backend {
        let tracks = Tracks {
            entries: vec![
                TrackEntry::new(1, 1, ids::track_type::VIDEO, "V_MPEG4/ISO/AVC")
                    .video(VideoSettings::new(64, 48)),
            ],
        };
        Backend::new(
            SinkAdapter(Box::new(output)),
            &crate::mkv::container_info(),
            &tracks,
        )
        .unwrap()
    }

    fn frame(index: u64) -> Frame<'static> {
        Frame {
            track: 1,
            timestamp_ns: index * 1_000_000,
            duration_ns: Some(1_000_000),
            data: &[0, 0, 0, 2, 0x65, 0x80],
            keyframe: index == 0,
            ..Frame::default()
        }
    }

    #[test]
    fn blocked_durable_sync_does_not_block_live_packets_or_claim_confirmation() {
        let (path, mut output) = file();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut first = true;
        inject_worker(&mut output, move |resources, _| {
            if first {
                first = false;
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            resources.file.sync_all()
        });
        let mut backend = backend(output);
        backend.write_frame(frame(0), false).unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let prefix = std::fs::metadata(&path).unwrap().len();
        assert!(backend.last_checkpoint().is_none());
        // These actual file writes must finish while the sync owner is blocked.
        for index in 1..=100 {
            backend.write_frame(frame(index), false).unwrap();
        }
        assert!(backend.last_checkpoint().is_none());
        release.send(()).unwrap();
        let confirmed = backend.checkpoint().unwrap();
        assert_eq!(confirmed.sequence, 2);
        assert!(confirmed.position_bytes > prefix);
        assert_eq!(confirmed.container_end_ns, 101_000_000);
        backend.finalize(&[1], None).unwrap().0.finish().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn asynchronous_checkpoint_failure_cannot_advance_or_finalize() {
        let (path, mut output) = file();
        inject_worker(&mut output, |_, _| {
            Err(io::Error::other("injected durable failure"))
        });
        let mut backend = backend(output);
        backend.write_frame(frame(0), false).unwrap();
        assert!(backend.checkpoint().is_err());
        assert!(backend.last_checkpoint().is_none());
        assert!(backend.write_frame(frame(1), false).is_err());
        assert!(backend.finalize(&[1], None).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn finish_waits_for_pending_sync_and_joins_its_owner() {
        let (path, mut output) = file();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        inject_worker(&mut output, move |resources, _| {
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(5)).unwrap();
            resources.file.sync_all()
        });
        output.write_all(b"complete prefix").unwrap();
        let point = super::super::DurableCheckpoint {
            sequence: 1,
            position_bytes: output.position(),
            container_end_ns: 1,
        };
        assert_eq!(output.request_checkpoint(point).unwrap(), None);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done, finished) = mpsc::channel();
        let join = thread::spawn(move || {
            done.send(Box::new(output).finish()).unwrap();
        });
        assert!(matches!(
            finished.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        join.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete prefix");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn abort_joins_even_an_unpolled_failure_and_requests_are_bounded() {
        let (path, mut output) = file();
        inject_worker(&mut output, |_, _| {
            Err(io::Error::other("injected unpolled failure"))
        });
        let point = super::super::DurableCheckpoint {
            sequence: 1,
            position_bytes: 0,
            container_end_ns: 1,
        };
        output.request_checkpoint(point).unwrap();
        assert!(
            output.request_checkpoint(point).is_err(),
            "one metadata request, no growing queue"
        );
        assert!(
            Box::new(output).finish().is_err(),
            "an unpolled IO failure prevents an artifact"
        );
        std::fs::remove_file(path).unwrap();
    }
}
