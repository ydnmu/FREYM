//! Opt-in, bounded development trace. No file I/O during sampled frames.
use std::{cell::RefCell, fmt::Write as _, path::PathBuf, time::Instant};

struct Trace {
    path: PathBuf,
    start: Instant,
    rows: Vec<[f64; 5]>,
    current: [f64; 5],
    finished: bool,
}
impl Trace {
    fn save(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut csv =
            String::from("callback_ms,cpu_frame_ms,encode_cpu_ms,present_cpu_ms,present_at_ms\n");
        for row in &self.rows {
            let _ = writeln!(
                csv,
                "{:.4},{:.4},{:.4},{:.4},{:.4}",
                row[0], row[1], row[2], row[3], row[4]
            );
        }
        if let Err(error) = std::fs::write(&self.path, csv) {
            eprintln!("Frame trace: {error}");
        }
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        self.save();
    }
}
thread_local! {
    static TRACE: RefCell<Option<Trace>> = RefCell::new(std::env::var_os("FREYM_FRAME_TRACE").map(|path| Trace {
        path: path.into(), start: Instant::now(), rows: Vec::with_capacity(512),
        current: [0.; 5], finished: false,
    }));
}
pub(crate) struct Frame(Instant);
pub(crate) fn begin() -> Option<Frame> {
    TRACE.with(|trace| {
        let mut trace = trace.borrow_mut();
        let trace = trace.as_mut().filter(|t| !t.finished)?;
        trace.current = [trace.start.elapsed().as_secs_f64() * 1000., 0., 0., 0., 0.];
        Some(Frame(Instant::now()))
    })
}
pub(crate) fn enabled() -> bool {
    TRACE.with(|t| t.borrow().as_ref().is_some_and(|t| !t.finished))
}
pub(crate) fn present(encode_ms: f64, present_ms: f64) {
    TRACE.with(|trace| {
        if let Some(trace) = trace.borrow_mut().as_mut().filter(|t| !t.finished) {
            trace.current[2] = encode_ms;
            trace.current[3] = present_ms;
            trace.current[4] = trace.start.elapsed().as_secs_f64() * 1000.;
        }
    });
}
impl Drop for Frame {
    fn drop(&mut self) {
        TRACE.with(|trace| {
            if let Some(trace) = trace.borrow_mut().as_mut().filter(|t| !t.finished) {
                trace.current[1] = self.0.elapsed().as_secs_f64() * 1000.;
                trace.rows.push(trace.current);
                // Stops after six seconds or 512 native callbacks, whichever is first.
                if trace.start.elapsed().as_secs() >= 6 || trace.rows.len() == 512 {
                    trace.save();
                }
            }
        });
    }
}
