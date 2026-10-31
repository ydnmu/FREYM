use crate::schema::*;
use crate::store::{create, encode};
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct MetadataOptions {
    pub queue_capacity: usize,
    pub max_bytes: u64,
}
impl Default for MetadataOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            max_bytes: 256 * 1024 * 1024,
        }
    }
}

struct Order {
    sequence: u64,
    closed: bool,
}
struct Shared {
    order: Mutex<Order>,
    dropped: AtomicU64,
    failed: AtomicBool,
    failure: Mutex<Option<String>>,
}
pub(crate) enum Message {
    Event(Event),
    End(bool),
    Flush(SyncSender<()>),
    Prune,
}

#[derive(Clone)]
pub struct EventIngress {
    tx: SyncSender<Message>,
    shared: Arc<Shared>,
    clock: Arc<MasterClock>,
    timeline: Arc<Mutex<PauseTimeline>>,
}
impl EventIngress {
    /// Nonblocking, best effort. Paused observations are intentionally omitted.
    /// Read the clock *inside* the timeline guard so pause/resume cannot race
    /// timestamp mapping. Native callbacks never wait for this guard or disk.
    pub fn try_emit(&self, event: EventKind) -> bool {
        let Ok(timeline) = self.timeline.try_lock() else {
            self.lost();
            return false;
        };
        if timeline.is_paused() {
            return false;
        }
        let raw = self.clock.now();
        let Some(mapped) = timeline.map(raw) else {
            self.lost();
            return false;
        };
        self.try_at(raw, mapped, event)
    }
    fn lost(&self) {
        self.shared.dropped.fetch_add(1, Ordering::Relaxed);
    }
    fn try_at(&self, raw: MediaTimestamp, mapped: MediaTimestamp, event: EventKind) -> bool {
        let Ok(mut order) = self.shared.order.try_lock() else {
            self.lost();
            return false;
        };
        if order.closed || self.shared.failed.load(Ordering::Acquire) {
            return false;
        }
        order.sequence += 1;
        let event = Event {
            version: SCHEMA_VERSION,
            sequence: order.sequence,
            raw_session_ns: raw.0,
            session_ns: mapped.0,
            event,
        };
        match self.tx.try_send(Message::Event(event)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.lost();
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

/// Owns the sole disk worker. Stop/join belongs on a recording control thread.
pub struct MetadataRecorder {
    ingress: EventIngress,
    worker: Option<JoinHandle<io::Result<()>>>,
    rolling: Option<crate::RollingMetadata>,
}
impl MetadataRecorder {
    pub fn start(
        path: &Path,
        clock: Arc<MasterClock>,
        timeline: PauseTimeline,
        header: Header,
        options: MetadataOptions,
    ) -> io::Result<Self> {
        if !(1..=16384).contains(&options.queue_capacity)
            || !(64 * 1024..=MAX_SIDECAR_BYTES).contains(&options.max_bytes)
            || header.version != SCHEMA_VERSION
        {
            return Err(invalid("Invalid metadata options or schema."));
        }
        let mut file = create(path)?;
        let header = Record::Header(header);
        let mut validator = Validator::default();
        validator.accept(&header)?;
        let bytes = encode(&header)?;
        file.write_all(&bytes)?;
        file.sync_all()?; // Header durable before session manifest references it.
        let written = bytes.len() as u64 + 8;
        let (tx, rx) = mpsc::sync_channel(options.queue_capacity);
        let shared = Arc::new(Shared {
            order: Mutex::new(Order {
                sequence: 0,
                closed: false,
            }),
            dropped: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("freym-metadata-writer".into())
            .spawn(move || {
                let result = (|| {
                    let mut written = written;
                    let mut events = 0;
                    let mut synced = Instant::now();
                    loop {
                        let message = match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(message) => Some(message),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Message::End(false)),
                        };
                        let end = matches!(message, Some(Message::End(_)));
                        let record = match message {
                            Some(Message::Event(event)) => {
                                events += 1;
                                Some(Record::Event(event))
                            }
                            Some(Message::End(completed)) => Some(Record::End {
                                completed,
                                events,
                                dropped: worker_shared.dropped.load(Ordering::Relaxed),
                            }),
                            Some(Message::Flush(ack)) => {
                                let _ = ack.send(());
                                None
                            }
                            Some(Message::Prune) => None,
                            None => None,
                        };
                        if let Some(record) = record {
                            validator.accept(&record)?;
                            let bytes = encode(&record)?;
                            written += bytes.len() as u64;
                            if written > options.max_bytes {
                                return Err(invalid(
                                    "Metadata disk budget reached; prefix preserved.",
                                ));
                            }
                            file.write_all(&bytes)?;
                        }
                        if end || synced.elapsed() >= Duration::from_secs(1) {
                            if !end {
                                let bytes = encode(&Record::Checkpoint {
                                    events,
                                    dropped: worker_shared.dropped.load(Ordering::Relaxed),
                                })?;
                                written += bytes.len() as u64;
                                if written > options.max_bytes {
                                    return Err(invalid(
                                        "Metadata disk budget reached; prefix preserved.",
                                    ));
                                }
                                file.write_all(&bytes)?;
                            }
                            file.sync_all()?;
                            synced = Instant::now();
                        }
                        if end {
                            return Ok(());
                        }
                    }
                })();
                if let Err(error) = &result {
                    *worker_shared
                        .failure
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                    worker_shared.failed.store(true, Ordering::Release);
                }
                result
            })?;
        Ok(Self {
            ingress: EventIngress {
                tx,
                shared,
                clock,
                timeline: Arc::new(Mutex::new(timeline)),
            },
            worker: Some(worker),
            rolling: None,
        })
    }
    pub fn start_rolling(
        clock: Arc<MasterClock>,
        timeline: PauseTimeline,
        header: Header,
        history_ns: u64,
    ) -> io::Result<Self> {
        let mut rolling = crate::RollingMetadata::new(header, history_ns)?;
        let (tx, rx) = mpsc::sync_channel(1024);
        rolling.barrier = Some(tx.clone());
        let store = rolling.clone();
        let shared = Arc::new(Shared {
            order: Mutex::new(Order {
                sequence: 0,
                closed: false,
            }),
            dropped: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("freym-replay-events".into())
            .spawn(move || {
                let result: io::Result<()> = (|| {
                    let mut validator = Validator::default();
                    validator.accept(&Record::Header(store.header.clone()))?;
                    while let Ok(message) = rx.recv() {
                        store.dropped.store(
                            worker_shared.dropped.load(Ordering::Relaxed),
                            Ordering::Relaxed,
                        );
                        match message {
                            Message::Event(e) => {
                                validator.accept(&Record::Event(e.clone()))?;
                                store.push(e)
                            }
                            Message::End(_) => break,
                            Message::Flush(ack) => {
                                let _ = ack.send(());
                            }
                            Message::Prune => store.prune(),
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = &result {
                    store.dropped.fetch_add(1, Ordering::Relaxed);
                    *worker_shared
                        .failure
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                    worker_shared.failed.store(true, Ordering::Release);
                }
                result
            })?;
        Ok(Self {
            ingress: EventIngress {
                tx,
                shared,
                clock,
                timeline: Arc::new(Mutex::new(timeline)),
            },
            worker: Some(worker),
            rolling: Some(rolling),
        })
    }
    /// Preserve the exact clock and private pause state when recording changes
    /// to replay-only. Called before packet admission, on the control owner.
    pub fn switch_to_rolling(&mut self, header: Header, history_ns: u64) -> io::Result<()> {
        let clock = self.ingress.clock.clone();
        let timeline = self
            .ingress
            .timeline
            .lock()
            .map_err(|_| invalid("Metadata pause lock failed."))?
            .clone();
        self.finish(false)?;
        *self = Self::start_rolling(clock, timeline, header, history_ns)?;
        self.lifecycle(Lifecycle::Started)
    }
    pub fn rolling(&self) -> Option<crate::RollingMetadata> {
        self.rolling.clone()
    }
    pub fn ingress(&self) -> EventIngress {
        self.ingress.clone()
    }
    /// The recorder owns a private pause snapshot. Acquire it before sampling
    /// the boundary clock, then apply that exact boundary to media and metadata.
    /// Native metadata producers never access the media mutex. Holding the
    /// private guard also prevents an observation crossing the transition.
    pub fn transition(&self, media: &mut PauseTimeline, state: Lifecycle) -> io::Result<()> {
        let mut metadata = self
            .ingress
            .timeline
            .lock()
            .map_err(|_| invalid("Metadata timeline lock failed."))?;
        let raw = self.ingress.clock.now();
        let apply = |timeline: &mut PauseTimeline| match state {
            Lifecycle::Paused => timeline.pause(raw),
            Lifecycle::Resumed => timeline.resume(raw),
            _ => None,
        };
        let mapped = apply(media).ok_or_else(|| invalid("Invalid media pause transition."))?;
        if apply(&mut metadata) != Some(mapped) {
            return Err(invalid("Metadata pause state differs from media."));
        }
        self.boundary(raw, mapped, EventKind::Lifecycle { state })
    }
    /// Control-only boundary. Caller holds the shared PauseTimeline guard and
    /// supplies the *same* raw/mapped values used for the media transition.
    pub fn boundary(
        &self,
        raw: MediaTimestamp,
        mapped: MediaTimestamp,
        event: EventKind,
    ) -> io::Result<()> {
        let mut order = self
            .ingress
            .shared
            .order
            .lock()
            .map_err(|_| invalid("Metadata order lock failed."))?;
        if order.closed {
            return Err(invalid("Metadata is closed."));
        }
        self.check()?;
        order.sequence += 1;
        match self.ingress.tx.try_send(Message::Event(Event {
            version: SCHEMA_VERSION,
            sequence: order.sequence,
            raw_session_ns: raw.0,
            session_ns: mapped.0,
            event,
        })) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.ingress.lost();
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(invalid("Metadata writer stopped.")),
        }
    }
    pub fn lifecycle(&self, state: Lifecycle) -> io::Result<()> {
        let timeline = self
            .ingress
            .timeline
            .lock()
            .map_err(|_| invalid("Metadata timeline lock failed."))?;
        let raw = self.ingress.clock.now();
        let mapped = timeline
            .map(raw)
            .ok_or_else(|| invalid("Metadata timestamp overflow."))?;
        self.boundary(raw, mapped, EventKind::Lifecycle { state })
    }
    pub fn check(&self) -> io::Result<()> {
        if self.ingress.shared.failed.load(Ordering::Acquire) {
            return Err(io::Error::other(
                self.ingress
                    .shared
                    .failure
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .unwrap_or_else(|| "Metadata writer failed.".into()),
            ));
        }
        Ok(())
    }
    pub fn finish(&mut self, completed: bool) -> io::Result<()> {
        let Some(worker) = self.worker.take() else {
            return self.check();
        };
        let lifecycle = self.lifecycle(if completed {
            Lifecycle::Completed
        } else {
            Lifecycle::Interrupted
        });
        let mut order = self
            .ingress
            .shared
            .order
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        order.closed = true;
        let _ = self
            .ingress
            .tx
            .send(Message::End(completed && lifecycle.is_ok()));
        drop(order);
        let joined = worker
            .join()
            .map_err(|_| io::Error::other("Metadata writer panicked."))?;
        joined.and(lifecycle)
    }
}
impl Drop for MetadataRecorder {
    fn drop(&mut self) {
        let _ = self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_or_contended_ingress_never_waits_and_reports_loss() {
        let (tx, rx) = mpsc::sync_channel(1);
        let timeline = Arc::new(Mutex::new(PauseTimeline::default()));
        let ingress = EventIngress {
            tx,
            timeline: timeline.clone(),
            clock: Arc::new(MasterClock::default()),
            shared: Arc::new(Shared {
                order: Mutex::new(Order {
                    sequence: 0,
                    closed: false,
                }),
                dropped: AtomicU64::new(0),
                failed: AtomicBool::new(false),
                failure: Mutex::new(None),
            }),
        };
        assert!(ingress.try_emit(EventKind::CursorLeft));
        assert!(!ingress.try_emit(EventKind::CursorLeft));
        let guard = timeline.lock().unwrap();
        assert!(!ingress.try_emit(EventKind::CursorLeft));
        drop(guard);
        assert_eq!(ingress.dropped(), 2);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
