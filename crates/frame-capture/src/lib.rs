//! Native implementations retain ownership of their GPU surfaces. No CPU copy
//! is implied by this interface. Platform handles never enter IPC contracts.
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{CaptureTarget, DisplayInfo, FpsMode, RecorderError, WindowInfo};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub trait DisplayBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError>;
    fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, RecorderError>;
}

pub struct CapturedFrame<S> {
    pub surface: S,
    pub timestamp: MediaTimestamp,
    pub width: u32,
    pub height: u32,
}

/// One-way cancellation shared by the coordinator and a single start/session.
/// Allocate a fresh token for a new session; clones cancel the same session.
#[derive(Clone, Default)]
pub struct CaptureCancellation(Arc<AtomicBool>);
impl CaptureCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The coordinator creates one master clock for the recording and injects it
/// into every source. Native backends must not create their own session epoch.
pub struct CaptureContext {
    pub clock: Arc<MasterClock>,
    pub cancellation: CaptureCancellation,
}

/// Coalescing wakeup for the recording owner. Notifications carry no frames and
/// allocate nothing; unpark's token survives a notification before park.
#[derive(Clone, Default)]
pub struct PipelineWake(Arc<std::sync::OnceLock<std::thread::Thread>>);

impl PipelineWake {
    pub fn for_current_thread() -> Self {
        let signal = Self::default();
        let _ = signal.0.set(std::thread::current());
        signal
    }

    pub fn notify(&self) {
        if let Some(owner) = self.0.get() {
            owner.unpark();
        }
    }
}

/// Native presentation identities, never pixel-hash uniqueness or nominal FPS.
/// Capture wrappers forward their upstream statistics without inventing counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapturePipelineStats {
    pub source_presentations: u64,
    pub captured_frames: u64,
    /// Completed copies of distinct native desktop presentations. Cursor-only
    /// composites are excluded; this requires no per-frame pixel readback/hash.
    pub captured_unique_frames: u64,
    pub pool_pressure_drops: u64,
    pub ready_queue_drops: u64,
    pub contention_drops: u64,
    pub upstream_accumulated_updates: u64,
    pub ready_depth: Option<u32>,
    pub ready_capacity: u32,
}

pub trait CaptureSession {
    type Surface;
    /// An unfilled, owned surface for device/format preparation only. A backend
    /// returning one must defer acquisition until `begin_frames`; it is never
    /// a captured frame and must never be submitted as encoder input.
    fn preparation_surface(&mut self) -> Result<Option<Self::Surface>, RecorderError> {
        Ok(None)
    }
    /// Arm a prepared source after its downstream owners are ready. Existing
    /// immediate sources need no handshake.
    fn begin_frames(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
    /// Attach before polling live media. Unsupported backends use the bounded
    /// owner fallback poll; notifications never replace the queue predicate.
    fn set_wake_signal(&mut self, _wake: PipelineWake) {}
    /// Opt-in still-image sampling for continuous replay. These samples are
    /// separate from native source presentation identities.
    fn set_replay_sampling(&mut self, _enabled: bool) {}
    fn pipeline_stats(&self) -> Option<CapturePipelineStats> {
        None
    }
    /// Nonblocking dequeue. None means no ready source update, including queue
    /// contention. Native updates are not proof of pixel-distinct frames.
    /// A terminal worker error must be surfaced, never replaced by perpetual None.
    fn try_frame(&mut self) -> Result<Option<CapturedFrame<Self::Surface>>, RecorderError>;
    /// Idempotent teardown/join on a control thread, never on the UI thread.
    /// Outstanding owned surfaces remain valid until their consumers release them.
    fn stop(&mut self) -> Result<(), RecorderError>;
}

pub trait CaptureBackend {
    type Session: CaptureSession;
    fn start(
        &mut self,
        target: &CaptureTarget,
        fps: FpsMode,
        context: CaptureContext,
    ) -> Result<Self::Session, RecorderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_reaches_clones_but_not_the_next_session() {
        let token = CaptureCancellation::default();
        let worker = token.clone();
        std::thread::spawn(move || worker.cancel()).join().unwrap();
        assert!(token.is_cancelled());
        assert!(!CaptureCancellation::default().is_cancelled());
    }
    #[test]
    fn wake_before_park_is_retained_and_requires_no_frame_queue() {
        let (ready, receive) = std::sync::mpsc::sync_channel(1);
        let gate = Arc::new(AtomicBool::new(false));
        let owner_gate = Arc::clone(&gate);
        let (done, result) = std::sync::mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            let wake = PipelineWake::for_current_thread();
            ready.send(wake).unwrap();
            while !owner_gate.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            std::thread::park();
            done.send(()).unwrap();
        });
        let wake = receive.recv().unwrap();
        wake.notify();
        wake.notify();
        gate.store(true, Ordering::Release);
        result
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        owner.join().unwrap();
    }
}
