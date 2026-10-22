//! Test-only byte target shared by the Matroska writer unit tests.

use super::output::OutputTarget;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Fails every write that would take the target past its current `limit`, so a
/// writer's failure policy can be exercised without filling a disk.
///
/// The limit is shared and re-settable, because a Matroska header and first packet
/// legitimately need room before the failure is injected. `limit()` reports the bytes
/// already accepted, which is the exact size a test can build its next limit from
/// instead of hard-coding container sizes.
#[derive(Clone)]
pub(super) struct FailingTarget {
    state: Arc<Mutex<State>>,
}

struct State {
    bytes: Vec<u8>,
    limit: usize,
    checkpoints: u64,
    fail_checkpoint: bool,
    fail_patches: bool,
}

impl FailingTarget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                bytes: Vec::new(),
                limit,
                checkpoints: 0,
                fail_checkpoint: false,
                fail_patches: false,
            })),
        }
    }

    /// Bytes the target has accepted so far.
    pub(super) fn limit(&self) -> usize {
        self.state.lock().expect("target state").bytes.len()
    }

    /// Replaces the ceiling. The next write that would exceed it fails.
    pub(super) fn set_limit(&self, limit: usize) {
        self.state.lock().expect("target state").limit = limit;
    }

    /// Every byte this target accepted, so a test can inspect what was really flushed.
    pub(super) fn bytes(&self) -> Vec<u8> {
        self.state.lock().expect("target state").bytes.clone()
    }

    pub(super) fn checkpoints(&self) -> u64 {
        self.state.lock().expect("target state").checkpoints
    }

    pub(super) fn fail_checkpoint(&self) {
        self.state.lock().expect("target state").fail_checkpoint = true;
    }

    pub(super) fn fail_patches(&self, fail: bool) {
        self.state.lock().expect("target state").fail_patches = fail;
    }
}

impl Write for FailingTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().expect("target state");
        if state.bytes.len() + buf.len() > state.limit {
            return Err(io::Error::other("injected target failure"));
        }
        state.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl OutputTarget for FailingTarget {
    fn position(&self) -> u64 {
        self.state.lock().expect("target state").bytes.len() as u64
    }
    fn seekable(&self) -> bool {
        true
    }
    fn patch(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().expect("target state");
        if state.fail_patches {
            return Err(io::Error::other("injected patch failure"));
        }
        let start = usize::try_from(offset).map_err(|_| io::Error::other("offset"))?;
        let end = start + buf.len();
        if end > state.bytes.len() {
            return Err(io::Error::other("patch outside the target"));
        }
        state.bytes[start..end].copy_from_slice(buf);
        Ok(())
    }
    fn finish(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().expect("target state");
        if state.fail_checkpoint {
            return Err(io::Error::other("injected flush/sync checkpoint failure"));
        }
        state.checkpoints += 1;
        Ok(())
    }
}
