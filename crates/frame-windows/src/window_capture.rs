//! Isolated Windows.Graphics.Capture source for a selected HWND.
//!
//! The desktop owner opts into this source explicitly for an eligible native SDR
//! window. The helper runs in a separate process; no display fallback is used.

#[allow(unsafe_code)]
mod native;
#[allow(unsafe_code)]
mod process;

pub(crate) fn wgc_helper_entry() -> i32 {
    process::run_helper()
}

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use frame_capture::{CaptureContext, CaptureSession, CapturedFrame};
use frame_contracts::{ErrorCode, FpsMode, GameTarget, RecorderError, RecoveryStatus, WindowId};

use crate::capture::{CaptureOptions, WindowsCaptureSession, start_source_with_factory};
use crate::surface::{WindowsSurface, plan_owned_payload};
use crate::window_discovery::{WindowIdentity, WindowStart};

pub const WINDOW_FRAME_POOL_BUFFERS: i32 = 3;

/// Start-time facts for the exact WGC identity and the adapter chosen by its
/// isolated helper. The first captured lease is checked again by the encoder.
pub struct WindowRecordingSource {
    pub window_id: WindowId,
    /// Fixed recording canvas. Resized source content is fitted on the GPU.
    pub width: u32,
    pub height: u32,
    pub adapter: crate::NativeAdapterIdentity,
}

fn encodable_geometry(source: (u32, u32)) -> Result<(u32, u32), RecorderError> {
    let output = (source.0 & !1, source.1 & !1);
    if output.0 == 0 || output.1 == 0 {
        return Err(RecorderError::unsupported(
            "The selected window is too small for 8-bit 4:2:0 recording.",
        ));
    }
    Ok(output)
}

pub fn recording_source(window_id: &WindowId) -> Result<WindowRecordingSource, RecorderError> {
    let identity = WindowIdentity::parse(&window_id.0)?;
    let state = identity.start_observation()?;
    identity.require_capturable(state.cloaked)?;
    let source_geometry = state.frame_size.ok_or_else(|| RecorderError {
        code: ErrorCode::SourceLost,
        message: format!(
            "The selected window {} has no usable frame bounds.",
            window_id.0
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Refresh sources and select a visible window.".into(),
    })?;
    let (width, height) = encodable_geometry(source_geometry)?;
    let (_, luid, _) = native::parent_device()?;
    let adapter = crate::enumerate_native_adapters()?
        .into_iter()
        .find(|candidate| candidate.luid == luid)
        .ok_or_else(|| {
            RecorderError::unsupported("The WGC helper adapter is unavailable for encoding.")
        })?;
    Ok(WindowRecordingSource {
        window_id: identity.id(),
        width,
        height,
        adapter,
    })
}

pub fn resolve_game_window(target: &GameTarget) -> Result<WindowId, RecorderError> {
    if target.process_id == 0 {
        return Err(RecorderError::invalid(
            "A windowed game needs a nonzero process ID.",
        ));
    }
    if let Some(identifier) = &target.app_identifier {
        let selected = WindowIdentity::parse(identifier)?;
        if selected.process_id() != target.process_id {
            return Err(RecorderError::invalid(
                "The game WindowId belongs to another process.",
            ));
        }
    }
    let windows: Vec<_> = crate::window_discovery::enumerate_windows()?
        .into_iter()
        .filter(|window| window.process_id == Some(target.process_id))
        .filter(|window| {
            target
                .app_identifier
                .as_ref()
                .is_none_or(|id| &window.id.0 == id)
        })
        .collect();
    match windows.as_slice() {
        [window] => Ok(window.id.clone()),
        [] => Err(RecorderError::unsupported(
            "The game process has no eligible windowed or borderless WGC target; exclusive fullscreen is unavailable.",
        )),
        _ => Err(RecorderError::invalid(
            "The game process has several eligible windows; choose one WindowId.",
        )),
    }
}
pub(crate) const MAX_FRAME_AGE: Duration = Duration::from_secs(1);
pub(crate) const MAX_FRAME_LEAD: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct TimestampBaseEvidence {
    pub(crate) ticks_per_second: u64,
    pub(crate) frame_units_per_second: u64,
    pub(crate) first_frame_ticks: i64,
    pub(crate) sampled_ticks: i64,
}

impl TimestampBaseEvidence {
    pub(crate) fn age_ticks(&self) -> i64 {
        self.sampled_ticks.saturating_sub(self.first_frame_ticks)
    }

    pub(crate) fn age_milliseconds(&self) -> f64 {
        if self.ticks_per_second == 0 {
            return 0.0;
        }
        self.age_ticks() as f64 * 1000.0 / self.ticks_per_second as f64
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CapturePolicyEvidence {
    pub(crate) cursor_capture_enabled: Option<bool>,
    pub(crate) border_required: Option<bool>,
}

/// WGC capture of a window, including a windowed or borderless game. This is not
/// injected or exclusive-fullscreen game capture. General device stability is unverified.
pub struct WindowsWindowCaptureSession {
    inner: WindowsCaptureSession,
    identity: WindowIdentity,
    start_state: WindowStart,
    timestamp_base: Arc<OnceLock<TimestampBaseEvidence>>,
    policy: Arc<OnceLock<CapturePolicyEvidence>>,
    unpublishable_updates: Arc<AtomicU64>,
}

impl WindowsWindowCaptureSession {
    /// Starts the exact selected window through the isolated helper.
    pub fn start_selected(
        window_id: &WindowId,
        fps: FpsMode,
        context: CaptureContext,
    ) -> Result<Self, RecorderError> {
        Self::start_selected_with_preview(window_id, fps, context, None)
    }
    pub fn start_selected_with_preview(
        window_id: &WindowId,
        fps: FpsMode,
        context: CaptureContext,
        preview: Option<Arc<crate::preview::PreviewHub>>,
    ) -> Result<Self, RecorderError> {
        Self::start_selected_with_preview_cursor(window_id, fps, context, preview, true)
    }

    /// WGC owns cursor composition; the copied/encoded surface is never given
    /// a second software cursor. The preference travels to the isolated helper.
    pub fn start_selected_with_preview_cursor(
        window_id: &WindowId,
        fps: FpsMode,
        context: CaptureContext,
        preview: Option<Arc<crate::preview::PreviewHub>>,
        show_cursor: bool,
    ) -> Result<Self, RecorderError> {
        if fps != FpsMode::AutoMatchSource {
            return Err(RecorderError::unsupported(
                "WGC window capture currently requires source-matched FPS",
            ));
        }
        let identity = WindowIdentity::parse(&window_id.0)?;
        let start_state = identity.start_observation()?;
        identity.require_capturable(start_state.cloaked)?;
        let source_geometry = start_state.frame_size.ok_or_else(|| RecorderError {
            code: ErrorCode::SourceLost,
            message: format!(
                "The selected window {} has no usable physical frame bounds.",
                window_id.0
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Select a visible window and retry.".into(),
        })?;
        let output_geometry = encodable_geometry(source_geometry)?;
        let plan = plan_owned_payload(output_geometry.0, output_geometry.1, false)?;
        let timestamp_base = Arc::new(OnceLock::new());
        let policy = Arc::new(OnceLock::new());
        let unpublishable_updates = Arc::new(AtomicU64::new(0));
        let worker_base = Arc::clone(&timestamp_base);
        let worker_policy = Arc::clone(&policy);
        let worker_unpublishable = Arc::clone(&unpublishable_updates);
        let inner = start_source_with_factory(
            output_geometry,
            None,
            plan,
            context,
            CaptureOptions {
                show_cursor,
                ..CaptureOptions::default()
            },
            preview.map(|hub| crate::capture::PreviewSetup {
                config: hub.config(),
                hub: Some(hub),
            }),
            move |preparation| {
                process::open(
                    &identity,
                    source_geometry,
                    output_geometry,
                    &worker_base,
                    &worker_policy,
                    &worker_unpublishable,
                    preparation,
                )
            },
        )?;
        Ok(Self {
            inner,
            identity,
            start_state,
            timestamp_base,
            policy,
            unpublishable_updates,
        })
    }

    /// Existing diagnostic caller, using the same isolated session path.
    pub fn start_diagnostic(
        window_id: &WindowId,
        fps: FpsMode,
        context: CaptureContext,
    ) -> Result<Self, RecorderError> {
        Self::start_selected(window_id, fps, context)
    }

    /// Resolve a process to exactly one offered top-level window. An explicit WindowId
    /// in `app_identifier` disambiguates multi-window processes; no screen fallback.
    pub fn start_windowed_game_diagnostic(
        target: &GameTarget,
        fps: FpsMode,
        context: CaptureContext,
    ) -> Result<Self, RecorderError> {
        if target.process_id == 0 {
            return Err(RecorderError::invalid(
                "A windowed game target needs a nonzero process ID.",
            ));
        }
        if let Some(identifier) = &target.app_identifier {
            let selected = WindowIdentity::parse(identifier)?;
            if selected.process_id() != target.process_id {
                return Err(RecorderError::invalid(format!(
                    "The game target names process {}, but its selected WindowId belongs to process {}.",
                    target.process_id,
                    selected.process_id(),
                )));
            }
        }
        let windows: Vec<_> = crate::window_discovery::enumerate_windows()?
            .into_iter()
            .filter(|window| window.process_id == Some(target.process_id))
            .filter(|window| {
                target
                    .app_identifier
                    .as_ref()
                    .is_none_or(|id| &window.id.0 == id)
            })
            .collect();
        if windows.is_empty() {
            return Err(RecorderError {
                code: ErrorCode::NotImplemented,
                message: format!(
                    "Process {} has no eligible WGC top-level window. Exclusive-fullscreen and protected game content have no supported capture path or display fallback.",
                    target.process_id,
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Use a visible windowed or borderless game window supported by Windows.Graphics.Capture.".into(),
            });
        }
        let [window] = windows.as_slice() else {
            return Err(RecorderError::invalid(format!(
                "Process {} resolved to {} WGC windows; select a unique window identity.",
                target.process_id,
                windows.len(),
            )));
        };
        Self::start_diagnostic(&window.id, fps, context)
    }

    pub fn window_id(&self) -> WindowId {
        self.identity.id()
    }

    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        let mut report = self.inner.diagnostics()?;
        report.push_str(&format!(
            "window: {}\n{}\nunpublishable_updates: {}\n",
            self.identity.id().0,
            self.start_state.describe(),
            self.unpublishable_updates.load(Ordering::Relaxed)
        ));
        report.push_str("capture_mode: native WGC window; child HWNDs crop their verified root's WGC surface; no desktop-region fallback\nresize_policy: GPU aspect fit into the initial recording canvas; capture session and encoder remain live\n");
        if let Some(base) = self.timestamp_base.get() {
            report.push_str(&format!(
                "window_time_base: {}/s; first frame age ticks={}\n",
                base.frame_units_per_second,
                base.sampled_ticks.saturating_sub(base.first_frame_ticks)
            ));
        }
        if let Some(policy) = self.policy.get() {
            report.push_str(&format!(
                "window_policy: cursor={:?} border={:?}\n",
                policy.cursor_capture_enabled, policy.border_required
            ));
        }
        Ok(report)
    }
}

impl CaptureSession for WindowsWindowCaptureSession {
    type Surface = WindowsSurface;
    fn set_wake_signal(&mut self, wake: frame_capture::PipelineWake) {
        self.inner.set_wake_signal(wake);
    }
    fn pipeline_stats(&self) -> Option<frame_capture::CapturePipelineStats> {
        self.inner.pipeline_stats()
    }
    fn try_frame(&mut self) -> Result<Option<CapturedFrame<Self::Surface>>, RecorderError> {
        self.inner.try_frame()
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        self.inner.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::encodable_geometry;

    #[test]
    fn odd_window_edges_are_cropped_only_at_right_and_bottom() {
        assert_eq!(encodable_geometry((1924, 1279)).unwrap(), (1924, 1278));
        assert_eq!(encodable_geometry((1925, 1279)).unwrap(), (1924, 1278));
        assert_eq!(encodable_geometry((1920, 1080)).unwrap(), (1920, 1080));
        assert!(encodable_geometry((1, 1080)).is_err());
    }
}
