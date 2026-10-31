// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Private control flow shared by the real worker and fault-injection tests.
use super::{internal, native};
use frame_contracts::RecorderError;

/// Native Start/Stop bookkeeping. Prepared clients have nothing to stop.
#[derive(Default)]
pub(super) struct CaptureLifecycle {
    started: bool,
    finished: bool,
}

impl CaptureLifecycle {
    pub(super) fn start(
        &mut self,
        start: impl FnOnce() -> Result<(), RecorderError>,
    ) -> Result<(), RecorderError> {
        if self.started || self.finished {
            return Err(internal(
                "Invalid microphone Start transition.".into(),
                "Start a new session.",
            ));
        }
        start()?;
        self.started = true;
        Ok(())
    }

    pub(super) fn running(&self) -> bool {
        self.started && !self.finished
    }

    pub(super) fn finish(
        &mut self,
        stop: impl FnOnce() -> Result<(), RecorderError>,
    ) -> Result<(), RecorderError> {
        let should_stop = self.running();
        // Mark before invoking: even an error must not lead to a Drop retry.
        self.finished = true;
        if should_stop { stop() } else { Ok(()) }
    }
}

/// Only the native operations the streaming worker needs; no platform resource
/// escapes through this private seam. Tests implement it without opening a device.
pub(super) trait CaptureStream {
    fn allows_packet_gaps(&self) -> bool {
        false
    }

    /// Process loopback has no hardware frame-position counter. This is a native
    /// backend policy, never inferred from a zero observed value.
    fn device_position_available(&self) -> bool {
        true
    }
    fn next_packet(
        &self,
        samples: &mut Vec<f32>,
        wait_ms: u32,
    ) -> Result<Option<native::PacketObservation>, RecorderError>;
    fn finish(&mut self) -> Result<(), RecorderError>;
    /// A stream that cannot separately prove each acquired buffer's successful
    /// release must not certify an automatic native continuity transition.
    fn packet_releases_succeeded(&self) -> bool {
        false
    }
}

pub(super) fn panic_error() -> RecorderError {
    internal(
        "The microphone worker panicked, so this source is failed.".into(),
        "Start a new microphone session.",
    )
}

/// Owns the started stream until body, fallible Stop and resource Drop finish.
/// All ordinary errors and panics in the streaming body converge here.
#[cfg(test)]
pub(super) fn run_started<C: CaptureStream>(
    capture: C,
    body: impl FnOnce(&C) -> Result<(), RecorderError>,
) -> Result<(), RecorderError> {
    run_started_with_release(capture, body).0
}

/// Returns release proof only after the fallible Stop and resource Drop. A
/// compound SourceLost error retains its code but never fabricates this proof.
pub(super) fn run_started_with_release<C: CaptureStream>(
    mut capture: C,
    body: impl FnOnce(&C) -> Result<(), RecorderError>,
) -> (Result<(), RecorderError>, bool) {
    let primary = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&capture)));
    let no_unwind = primary.is_ok();
    let primary = primary.unwrap_or_else(|_| Err(panic_error()));
    let cleanup = capture.finish();
    let released = no_unwind && cleanup.is_ok() && capture.packet_releases_succeeded();
    // Drops the interfaces/event before returning to the apartment owner, even
    // when Stop fails. The native lifecycle prevents a second Stop from Drop.
    drop(capture);
    let result = match (primary, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(primary), Err(cleanup)) => Err(native::combine(primary, cleanup)),
    };
    (result, released)
}
