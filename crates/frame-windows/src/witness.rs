#![allow(unsafe_code)] // Isolated Frame-owned C ABI with checked lifetimes and bounds.
//! Bounded per-session native call observations. Records are copied under a try-lock;
//! a snapshot never follows a resource pointer or holds a lock across a native call.
use frame_contracts::RecorderError;
use std::ffi::{CStr, c_char};

/// The bridge's bounded call-phase record: the NVENC API it is inside right now.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FrameNvencPhase {
    pub(crate) api: [c_char; 64],
    pub(crate) session_id: u64,
    pub(crate) started_qpc: u64,
    pub(crate) ended_qpc: u64,
    pub(crate) observed_qpc: u64,
    pub(crate) ticks_per_second: u64,
    pub(crate) thread_id: u32,
    pub(crate) running: u32,
    pub(crate) completed_calls: u32,
    pub(crate) reserved: u32,
}

impl FrameNvencPhase {
    pub(crate) fn zeroed() -> Self {
        // SAFETY: plain integers and a byte array with no invalid bit patterns.
        unsafe { std::mem::zeroed() }
    }

    /// The API name and, when one is in progress, how long it has been running.
    pub(crate) fn describe(&self) -> String {
        // SAFETY: the bridge always NUL-terminates `api` inside its bounded array.
        let name = unsafe { CStr::from_ptr(self.api.as_ptr()) }.to_string_lossy();
        let end = if self.running == 1 {
            self.observed_qpc
        } else {
            self.ended_qpc
        };
        let elapsed = end.saturating_sub(self.started_qpc);
        format!(
            "session={} api={} running={} thread={} start_qpc={} observed_qpc={} elapsed_ticks={} qpf={} completed={}",
            self.session_id,
            name,
            self.running,
            self.thread_id,
            self.started_qpc,
            self.observed_qpc,
            elapsed,
            self.ticks_per_second,
            self.completed_calls
        )
    }
}

unsafe extern "C" {
    fn frame_witness_create(initial: *const c_char) -> u64;
    fn frame_witness_release(id: u64);
    fn frame_witness_begin(id: u64, api: *const c_char);
    fn frame_witness_end(id: u64);
    fn frame_witness_snapshots(records: *mut FrameNvencPhase, capacity: u32) -> u32;
}

/// Exclusive owner of one registry slot. Generation IDs prevent reuse confusion.
/// The C bridge owns encoder slots; capture holds this owner through native teardown.
pub(crate) struct Witness(u64);
impl Witness {
    pub(crate) fn new() -> Result<Self, RecorderError> {
        // SAFETY: static NUL-terminated text; the bridge copies it before returning.
        let id = unsafe { frame_witness_create(c"capture/preparation-unknown".as_ptr()) };
        if id == 0 {
            return Err(crate::encode::internal_error(
                "The bounded 64-session witness registry is exhausted.".to_string(),
            ));
        }
        Ok(Self(id))
    }
    pub(crate) fn enter(&self, api: &'static CStr) -> Phase<'_> {
        // SAFETY: this owner keeps its generation slot alive; text is NUL-terminated.
        unsafe { frame_witness_begin(self.0, api.as_ptr()) };
        Phase(self)
    }
}
impl Drop for Witness {
    fn drop(&mut self) {
        // SAFETY: releases only this owner's generation, exactly once.
        unsafe { frame_witness_release(self.0) };
    }
}
pub(crate) struct Phase<'a>(&'a Witness);
impl Drop for Phase<'_> {
    fn drop(&mut self) {
        // SAFETY: the borrowed owner outlives this phase, including unwind.
        unsafe { frame_witness_end(self.0.0) };
    }
}
fn snapshots() -> Option<Vec<FrameNvencPhase>> {
    let mut records = [FrameNvencPhase::zeroed(); 64];
    // SAFETY: capacity is exactly the initialized array size; C copies under its lock.
    let count = unsafe { frame_witness_snapshots(records.as_mut_ptr(), 64) };
    if count == u32::MAX {
        None
    } else {
        Some(records[..count as usize].to_vec())
    }
}
pub(crate) fn describe_all() -> String {
    match snapshots() {
        None => "unknown: witness snapshot contended".to_string(),
        Some(records) if records.is_empty() => {
            "no live native sessions; prior phases unavailable".to_string()
        }
        Some(records) => records
            .iter()
            .map(FrameNvencPhase::describe)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_sessions_keep_coherent_snapshots_through_teardown() {
        let first = Witness::new().unwrap();
        let second = Witness::new().unwrap();
        let first_id = first.0;
        let second_id = second.0;
        let running = first.enter(c"test/stuck-call");
        let worker = std::thread::spawn(move || {
            for _ in 0..2000 {
                let _call = second.enter(c"test/other-session");
            }
            second
        });
        let mut observed = 0;
        for _ in 0..2000 {
            if let Some(records) = snapshots() {
                let record = records.iter().find(|r| r.session_id == first_id).unwrap();
                assert_eq!(record.running, 1);
                assert_eq!(
                    unsafe { CStr::from_ptr(record.api.as_ptr()) },
                    c"test/stuck-call"
                );
                assert!(record.ticks_per_second > 0 && record.started_qpc > 0);
                assert!(record.observed_qpc >= record.started_qpc);
                observed += 1;
            }
        }
        let second = worker.join().unwrap();
        assert_ne!(first_id, second_id);
        assert!(observed > 0);
        drop(running);
        let retained_copy = loop {
            if let Some(records) = snapshots() {
                break records;
            }
        };
        drop(first);
        drop(second);
        assert!(
            retained_copy.iter().any(|r| r.session_id == first_id
                && r.running == 0
                && r.ended_qpc >= r.started_qpc)
        );
        if let Some(records) = snapshots() {
            assert!(
                !records
                    .iter()
                    .any(|r| r.session_id == first_id || r.session_id == second_id)
            );
        }
    }
}
