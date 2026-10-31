//! Bounded replay events, with small pins for queued disk jobs. No native callback
//! writes files or copies the rolling history. Only the existing worker drains it.
use crate::schema::*;
use crate::store::{create, encode};
use std::{
    collections::VecDeque,
    io::{self, Write},
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Default)]
pub(crate) struct Window {
    events: VecDeque<Event>,
    geometry: Option<Event>,
    pointer: Option<Event>,
    lost: u64,
}
#[derive(Clone)]
pub struct RollingMetadata {
    pub(crate) header: Header,
    pub(crate) window: Arc<Mutex<Window>>,
    pub(crate) floor: Arc<AtomicU64>,
    pins: Arc<Mutex<Vec<Weak<Range>>>>,
    history_ns: u64,
    pub(crate) barrier: Option<std::sync::mpsc::SyncSender<crate::writer::Message>>,
    pub(crate) dropped: Arc<AtomicU64>,
}
struct Range {
    start: u64,
    end: u64,
}
pub struct ReplayMetadataSnapshot {
    store: RollingMetadata,
    range: Arc<Range>,
}
impl RollingMetadata {
    pub(crate) fn new(header: Header, history_ns: u64) -> io::Result<Self> {
        if !(1_000_000_000..=600_000_000_000).contains(&history_ns)
            || header.version != SCHEMA_VERSION
        {
            return Err(invalid("Invalid rolling metadata duration/schema."));
        }
        Ok(Self {
            header,
            window: Arc::new(Mutex::new(Window::default())),
            floor: Arc::new(AtomicU64::new(0)),
            pins: Arc::new(Mutex::new(Vec::new())),
            history_ns,
            barrier: None,
            dropped: Arc::new(AtomicU64::new(0)),
        })
    }
    /// O(1) on the media owner. The event worker prunes at its next drain.
    pub fn retain_from(&self, start: u64) {
        self.floor.fetch_max(start, Ordering::Release);
        if let Some(tx) = &self.barrier {
            let _ = tx.try_send(crate::writer::Message::Prune);
        } else {
            self.prune();
        }
    }
    /// Pin the selected encoded range before queueing I/O, so job latency cannot
    /// erase its events. Contention is explicit; it never waits on the media owner.
    pub fn pin(&self, start: u64, end: u64) -> io::Result<ReplayMetadataSnapshot> {
        if start >= end {
            return Err(invalid("Empty replay metadata range."));
        }
        let mut pins = self
            .pins
            .try_lock()
            .map_err(|_| invalid("Replay metadata snapshot is busy; retry saving."))?;
        pins.retain(|p| p.strong_count() > 0);
        if pins.len() >= 4 {
            return Err(invalid("Too many pending replay metadata snapshots."));
        }
        let range = Arc::new(Range { start, end });
        pins.push(Arc::downgrade(&range));
        Ok(ReplayMetadataSnapshot {
            store: self.clone(),
            range,
        })
    }
    pub(crate) fn push(&self, event: Event) {
        let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
        let now = event.session_ns;
        window.events.push_back(event);
        self.prune_window(&mut window, now);
    }
    pub(crate) fn prune(&self) {
        let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
        let now = window.events.back().map_or(0, |e| e.session_ns);
        self.prune_window(&mut window, now);
    }
    fn prune_window(&self, window: &mut Window, now: u64) {
        // Covers history plus one seal/GOP overlap even while disk work is delayed.
        let mut floor = self
            .floor
            .load(Ordering::Acquire)
            .max(now.saturating_sub(self.history_ns + 62_080_000_000));
        let pins = self.pins.lock().unwrap_or_else(|p| p.into_inner());
        for pin in pins.iter().filter_map(Weak::upgrade) {
            floor = floor.min(pin.start);
        }
        while window.events.front().is_some_and(|e| e.session_ns < floor)
            || window.events.len() > 100_000
        {
            let mut expired = window.events.pop_front().unwrap();
            if expired.session_ns >= floor {
                window.lost += 1;
            }
            match expired.event {
                EventKind::Geometry { .. } => window.geometry = Some(expired),
                EventKind::Button { x, y, .. } | EventKind::Scroll { x, y, .. } => {
                    expired.event = EventKind::Cursor { x, y };
                    window.pointer = Some(expired)
                }
                EventKind::SourceUnavailable
                | EventKind::Focus { active: false }
                | EventKind::Loss { .. } => {
                    expired.event = EventKind::CursorLeft;
                    window.pointer = Some(expired)
                }
                EventKind::Cursor { .. } | EventKind::CursorLeft => window.pointer = Some(expired),
                _ => (),
            }
        }
    }
}
impl ReplayMetadataSnapshot {
    /// Off the media/GPUI threads. Carries state at the cut, but never replays an
    /// expired click/scroll. Original session clocks remain in the saved sidecar.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        self.write_range(path, self.range.start, self.range.end)
    }
    pub fn write_range(&self, path: &Path, start: u64, end: u64) -> io::Result<()> {
        if start < self.range.start || end > self.range.end || start >= end {
            return Err(invalid("Replay metadata range exceeds its pinned media."));
        }
        if let Some(tx) = &self.store.barrier {
            let (ack, rx) = std::sync::mpsc::sync_channel(1);
            // The observer can already be stopped at finalization; its drained
            // window stays readable after sender disconnection.
            if tx.send(crate::writer::Message::Flush(ack)).is_ok() {
                rx.recv_timeout(std::time::Duration::from_secs(2))
                    .map_err(|_| invalid("Replay event drain timed out; retry saving."))?;
            }
        }
        let window = self.store.window.lock().unwrap_or_else(|p| p.into_inner());
        let mut geometry = window.geometry.clone();
        let mut pointer = window.pointer.clone();
        for e in window.events.iter().take_while(|e| e.session_ns < start) {
            match e.event {
                EventKind::Geometry { .. } => geometry = Some(e.clone()),
                EventKind::Cursor { .. }
                | EventKind::CursorLeft
                | EventKind::Button { .. }
                | EventKind::Scroll { .. }
                | EventKind::SourceUnavailable
                | EventKind::Focus { active: false }
                | EventKind::Loss { .. } => pointer = Some(e.clone()),
                _ => (),
            }
        }
        let mut header = self.store.header.clone();
        header.origin_ns = start;
        geometry = geometry.filter(|e| e.session_ns <= start);
        pointer = pointer.filter(|e| {
            e.session_ns <= start
                && geometry
                    .as_ref()
                    .is_some_and(|g| g.raw_session_ns <= e.raw_session_ns)
        });
        let carry_raw = geometry
            .iter()
            .chain(pointer.iter())
            .map(|e| e.raw_session_ns)
            .max()
            .unwrap_or(start)
            .max(start);
        let mut events = Vec::new();
        for mut e in geometry.into_iter().chain(pointer) {
            if let EventKind::Button { x, y, .. } | EventKind::Scroll { x, y, .. } = e.event {
                e.event = EventKind::Cursor { x, y };
            }
            if matches!(
                e.event,
                EventKind::SourceUnavailable
                    | EventKind::Focus { active: false }
                    | EventKind::Loss { .. }
            ) {
                e.event = EventKind::CursorLeft;
            }
            e.session_ns = start;
            e.raw_session_ns = carry_raw;
            events.push(e);
        }
        events.extend(
            window
                .events
                .iter()
                .filter(|e| e.session_ns >= start && e.session_ns < end)
                .cloned(),
        );
        let lost = window.lost + self.store.dropped.load(Ordering::Relaxed);
        drop(window);
        // Carry records may have different raw clocks: order by mapped then raw.
        events.sort_by_key(|e| (e.session_ns, e.raw_session_ns));
        for (i, e) in events.iter_mut().enumerate() {
            e.sequence = i as u64 + 1;
        }
        let mut validator = Validator::default();
        validator.accept(&Record::Header(header.clone()))?;
        for event in &events {
            validator.accept(&Record::Event(event.clone()))?;
        }
        let mut file = create(path)?;
        let result = (|| {
            file.write_all(&encode(&Record::Header(header))?)?;
            for e in &events {
                file.write_all(&encode(&Record::Event(e.clone()))?)?;
            }
            file.write_all(&encode(&Record::End {
                completed: true,
                events: events.len() as u64,
                dropped: lost,
            })?)?;
            file.sync_all()
        })();
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}
