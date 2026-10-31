//! Opt-in, bounded cursor/preview evidence. No pixel readback or diagnostic UI.
//! Producers never wait for the writer; absent FREYM_CURSOR_PREVIEW_TRACE means
//! no file, queue or thread. Times are QPC nanoseconds, shared by WGC and its host.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

const QUEUE_CAPACITY: usize = 2048;
const MAX_EVENTS: u64 = 100_000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureTrace {
    pub source_id: u64,
    pub sequence: u64,
    pub acquired_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewFrameTrace {
    pub capture: CaptureTrace,
    pub native_ticks: i64,
    pub media_ns: Option<u64>,
    pub completed_ns: u64,
}

#[derive(serde::Serialize)]
struct Event {
    event: &'static str,
    at_ns: u64,
    process_id: u32,
    frame: Option<CaptureTrace>,
    fields: serde_json::Value,
}

struct Recorder {
    sender: SyncSender<Event>,
    stopped: AtomicBool,
    dropped: AtomicU64,
}

static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();
static FREQUENCY: OnceLock<u64> = OnceLock::new();
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);

pub fn enabled() -> bool {
    RECORDER
        .get()
        .is_some_and(|trace| !trace.stopped.load(Ordering::Relaxed))
}

pub fn new_source_id() -> u64 {
    (u64::from(std::process::id()) << 32) | NEXT_SOURCE.fetch_add(1, Ordering::Relaxed)
}

fn frequency() -> u64 {
    *FREQUENCY.get_or_init(|| {
        let mut value = 0;
        // SAFETY: QPF writes one owned i64 and has no handle/lifetime dependency.
        if unsafe { QueryPerformanceFrequency(&mut value) }.is_ok() && value > 0 {
            value as u64
        } else {
            0
        }
    })
}

pub fn now_ns() -> u64 {
    let mut ticks = 0;
    let hz = frequency();
    // SAFETY: QPC writes one owned i64. A failed clock is reported as zero.
    if hz == 0 || unsafe { QueryPerformanceCounter(&mut ticks) }.is_err() || ticks < 0 {
        return 0;
    }
    ((ticks as u128 * 1_000_000_000) / u128::from(hz)).min(u128::from(u64::MAX)) as u64
}

/// Fields are constructed only for an explicitly enabled trace. Event loss is
/// counted, so incomplete evidence cannot silently become a passing result.
pub fn record(
    event: &'static str,
    frame: Option<CaptureTrace>,
    fields: impl FnOnce() -> serde_json::Value,
) {
    let Some(trace) = RECORDER.get() else { return };
    if trace.stopped.load(Ordering::Relaxed) {
        return;
    }
    let row = Event {
        event,
        at_ns: now_ns(),
        process_id: std::process::id(),
        frame,
        fields: fields(),
    };
    if trace.sender.try_send(row).is_err() {
        trace.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Owns the sole diagnostic writer and joins it at orderly application exit.
pub struct TraceGuard {
    recorder: Arc<Recorder>,
    writer: Option<JoinHandle<io::Result<()>>>,
}

impl Drop for TraceGuard {
    fn drop(&mut self) {
        self.recorder.stopped.store(true, Ordering::Relaxed);
        if let Some(writer) = self.writer.take() {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("Cursor/preview trace could not finish: {error}"),
                Err(_) => {
                    eprintln!("Cursor/preview trace writer panicked; evidence is incomplete.")
                }
            }
        }
    }
}

/// Creates a new file, never truncates an existing artifact. The WGC helper gets
/// its own file and process identity; MF encoding has no diagnostic writer.
pub fn start_from_env() -> io::Result<Option<TraceGuard>> {
    let Some(path) = std::env::var_os("FREYM_CURSOR_PREVIEW_TRACE").filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };
    let helper = std::env::args().nth(1);
    if helper.as_deref() == Some("--frame-mf-helper") {
        return Ok(None);
    }
    let mut path = PathBuf::from(path);
    if helper.as_deref() == Some("--frame-wgc-helper") {
        let mut filename = path
            .file_name()
            .ok_or_else(|| io::Error::other("Trace requires a filename."))?
            .to_os_string();
        filename.push(format!(".wgc-{}.jsonl", std::process::id()));
        path.set_file_name(filename);
    }
    if RECORDER.get().is_some() {
        return Err(io::Error::other("Trace already started."));
    }
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let recorder = Arc::new(Recorder {
        sender,
        stopped: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
    });
    let writer_trace = recorder.clone();
    let writer = thread::Builder::new()
        .name("cursor-preview-trace".into())
        .spawn(move || {
            let result = write_trace(file, receiver, &writer_trace, MAX_EVENTS, MAX_BYTES);
            writer_trace.stopped.store(true, Ordering::Relaxed);
            result
        })?;
    if RECORDER.set(recorder.clone()).is_err() {
        recorder.stopped.store(true, Ordering::Relaxed);
        let _ = writer.join();
        return Err(io::Error::other("Trace already started."));
    }
    Ok(Some(TraceGuard {
        recorder,
        writer: Some(writer),
    }))
}

fn write_line(output: &mut impl Write, row: &impl serde::Serialize) -> io::Result<u64> {
    let bytes = serde_json::to_vec(row).map_err(io::Error::other)?;
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    Ok(bytes.len() as u64 + 1)
}

fn write_trace(
    output: impl Write,
    receiver: Receiver<Event>,
    trace: &Recorder,
    max_events: u64,
    max_bytes: u64,
) -> io::Result<()> {
    let mut output = BufWriter::new(output);
    let mut bytes = write_line(
        &mut output,
        &serde_json::json!({
            "event": "trace_start", "schema": 1, "process_id": std::process::id(),
            "clock": "QPC_nanoseconds", "qpc_frequency": frequency(),
            "at_ns": now_ns(), "queue_capacity": QUEUE_CAPACITY,
            "max_events": max_events, "max_bytes": max_bytes,
            "embedded_in_pixels": "unknown_without_pixel_evidence"
        }),
    )?;
    let mut written = 0;
    let mut budget_exhausted = false;
    loop {
        let row = if trace.stopped.load(Ordering::Relaxed) {
            match receiver.try_recv() {
                Ok(row) => row,
                Err(_) => break,
            }
        } else {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(row) => row,
                Err(RecvTimeoutError::Timeout) => {
                    output.flush()?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        if written >= max_events || bytes >= max_bytes {
            budget_exhausted = true;
            trace.stopped.store(true, Ordering::Relaxed);
            trace.dropped.fetch_add(1, Ordering::Relaxed);
            trace
                .dropped
                .fetch_add(receiver.try_iter().count() as u64, Ordering::Relaxed);
            break;
        }
        bytes += write_line(&mut output, &row)?;
        written += 1;
        if written % 256 == 0 {
            output.flush()?;
        }
    }
    write_line(
        &mut output,
        &serde_json::json!({
            "event": "trace_end", "at_ns": now_ns(), "process_id": std::process::id(),
            "written": written, "bytes_before_footer": bytes,
            "dropped_events": trace.dropped.load(Ordering::Relaxed), "budget_exhausted": budget_exhausted
        }),
    )?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64) -> Event {
        Event {
            event: "fixture",
            at_ns: now_ns(),
            process_id: std::process::id(),
            frame: Some(CaptureTrace {
                source_id: 1,
                sequence,
                acquired_ns: 1,
            }),
            fields: serde_json::json!({"composition_count": 1}),
        }
    }

    #[test]
    fn trace_budget_reports_truncation_and_drains_without_waiting() {
        let (sender, receiver) = mpsc::sync_channel(4);
        for sequence in 1..=3 {
            sender.try_send(event(sequence)).unwrap();
        }
        let trace = Recorder {
            sender,
            stopped: AtomicBool::new(true),
            dropped: AtomicU64::new(0),
        };
        let mut output = Vec::new();
        write_trace(&mut output, receiver, &trace, 1, u64::MAX).unwrap();
        let rows: Vec<serde_json::Value> = output
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["frame"]["sequence"], 1);
        assert_eq!(rows[2]["written"], 1);
        assert_eq!(rows[2]["dropped_events"], 2);
        assert_eq!(rows[2]["budget_exhausted"], true);
    }

    #[test]
    fn qpc_trace_clock_is_monotonic_and_source_ids_are_distinct() {
        let before = now_ns();
        assert!(before > 0);
        assert!(now_ns() >= before);
        assert_ne!(new_source_id(), new_source_id());
    }
}
