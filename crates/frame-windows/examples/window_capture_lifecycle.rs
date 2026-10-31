#![allow(unsafe_code)]
// The fixture window is this example's own bounded Win32 boundary, like the crate's native modules.

//! Window closure, hidden/minimized behaviour and joined cleanup for one real window
//! (FRAME-0204).
//!
//! It creates its **own** small top-level window (registered class, own message pump,
//! repainting on a timer), captures it with the real window session, and then drives the
//! states a capture has to survive or report:
//!
//! ```text
//! cargo run --locked -p frame-windows --example window_capture_lifecycle
//! ```
//!
//! 1. visible: captured frames arrive;
//! 2. minimized: no new frame arrives, and the session says so instead of inventing one;
//! 3. restored: frames arrive again;
//! 4. hidden: no new frame arrives;
//! 5. shown again: frames arrive again;
//! 6. destroyed: the session reports the closure as an explicit `SourceLost` within a
//!    bounded time. Windows.Graphics.Capture itself keeps answering "no frame" for a
//!    closed window, so this identity re-check is the closure signal the session relies
//!    on;
//! 7. stopped while one lease is still held across the close: the cleanup is joined, the
//!    lease stays valid, its slot returns to the pool when it is released, and the GPU
//!    payload is destroyed.
//!
//! No other process's window is touched: the example only ever creates, minimizes, hides
//! and destroys its own fixture, and it changes no capture privacy or permission setting.
//! The run exits non-zero if any check fails and prints what it observed either way.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-wgc-helper")
    {
        std::process::exit(frame_windows::run_wgc_helper());
    }
    lifecycle::run()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("Window capture is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod lifecycle {
    use std::io::Write;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use frame_capture::{
        CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame, DisplayBackend,
    };
    use frame_clock::MasterClock;
    use frame_contracts::{ErrorCode, FpsMode, RecorderError, RecoveryStatus, WindowId};
    use frame_windows::{PlatformBackend, WindowsSurface, WindowsWindowCaptureSession};

    /// The fixture's window title. The example finds its own window through the real
    /// window discovery, so the list it uses is the list a caller would select from.
    const FIXTURE_TITLE: &str = "FRAME-0204 fixture window";
    /// How long one state observation runs.
    const STATE_WINDOW: Duration = Duration::from_millis(700);
    /// How long the closure may take to be reported once the window is gone.
    const CLOSURE_BOUND: Duration = Duration::from_secs(5);
    /// How long the first captured frame may take.
    const FIRST_FRAME_BOUND: Duration = Duration::from_secs(10);
    const IDLE_PAUSE: Duration = Duration::from_millis(1);

    /// One phase's observation.
    struct Phase {
        name: &'static str,
        frames: u64,
        elapsed: Duration,
        /// The first terminal error this phase observed, if any.
        terminal: Option<ErrorCode>,
    }

    impl Phase {
        fn describe(&self) -> String {
            format!(
                "  {:<10} frames={} in {} ms terminal={:?}",
                self.name,
                self.frames,
                self.elapsed.as_millis(),
                self.terminal
            )
        }
    }

    struct Checks {
        passed: Vec<String>,
        failed: Vec<String>,
    }

    impl Checks {
        fn new() -> Self {
            Self {
                passed: Vec::new(),
                failed: Vec::new(),
            }
        }

        fn check(&mut self, condition: bool, description: &str) {
            if condition {
                self.passed.push(description.to_string());
            } else {
                self.failed.push(description.to_string());
            }
        }
    }

    fn internal_error(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::Internal,
            message,
            recovery: RecoveryStatus::Unknown,
            action: "Do not treat this run as evidence; report the failure.".to_string(),
        }
    }

    pub(super) fn run() -> ExitCode {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        if arguments.first().is_some_and(|arg| arg == "--fixture-hold") {
            let seconds = match arguments.as_slice() {
                [_, value] => value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| (1..=30).contains(value)),
                _ => None,
            };
            let Some(seconds) = seconds else {
                eprintln!("usage: window_capture_lifecycle --fixture-hold 1..30");
                return ExitCode::FAILURE;
            };
            return match hold_fixture(seconds) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("fixture hold failed: {}", error.message);
                    ExitCode::FAILURE
                }
            };
        }
        if !arguments.is_empty() {
            eprintln!("usage: window_capture_lifecycle [--fixture-hold 1..30]");
            return ExitCode::FAILURE;
        }
        match drive() {
            Ok((report, failures)) => {
                print!("{report}");
                if failures == 0 {
                    ExitCode::SUCCESS
                } else {
                    eprintln!("window_capture_lifecycle: {failures} check(s) failed.");
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("window_capture_lifecycle failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                ExitCode::FAILURE
            }
        }
    }

    /// Keeps only this example's repainting fixture alive for a bounded recording
    /// check in a second process. It captures nothing and touches no other window.
    fn hold_fixture(seconds: u64) -> Result<(), RecorderError> {
        let fixture = fixture::Fixture::spawn()?;
        let window = PlatformBackend
            .enumerate_windows()?
            .into_iter()
            .find(|window| {
                window.title == FIXTURE_TITLE && window.process_id == Some(std::process::id())
            })
            .ok_or_else(|| {
                internal_error(
                    "The own fixture window was not found in window discovery.".to_string(),
                )
            })?;
        println!("fixture_id={}", window.id.0);
        std::io::stdout().flush().map_err(|error| {
            internal_error(format!(
                "The fixture identity could not be flushed: {error}"
            ))
        })?;
        std::thread::sleep(Duration::from_secs(seconds));
        fixture.destroy();
        Ok(())
    }

    fn drive() -> Result<(String, usize), RecorderError> {
        let mut checks = Checks::new();
        let fixture = fixture::Fixture::spawn()?;
        println!(
            "THREADS main={} pump={}",
            unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() },
            fixture.thread_id_for_diagnostics()
        );
        let mut report = String::new();
        report.push_str("Frame Windows window capture lifecycle (FRAME-0204)\n");
        report.push_str(
            "This run captures a locally created fixture window; no other process's window is modified and no privacy setting is changed.\n",
        );

        // The fixture is found through the real discovery, which also proves it is a real
        // capturable top-level window and yields the identity a caller would select.
        let windows = PlatformBackend.enumerate_windows()?;
        let fixture_window = windows
            .iter()
            .find(|window| {
                window.title == FIXTURE_TITLE && window.process_id == Some(std::process::id())
            })
            .cloned()
            .ok_or_else(|| {
                let listed: String = windows
                    .iter()
                    .filter(|window| window.process_id == Some(std::process::id()))
                    .map(|window| format!("\n  {} title={:?}", window.id.0, window.title))
                    .collect();
                internal_error(format!(
                    "The fixture window {FIXTURE_TITLE:?} is not in the discovered window list, so the lifecycle run cannot select it. Windows discovered for this process: {listed}"
                ))
            })?;
        report.push_str(&format!(
            "fixture: {} pid={:?} title={:?}\n",
            fixture_window.id.0, fixture_window.process_id, fixture_window.title
        ));

        // 0. Start validation. The identity is re-verified before any native resource
        // exists, so a stale handle, a recycled handle and a malformed identity are
        // refused instead of capturing whatever now answers to that handle.
        let handle_field = fixture_window
            .id
            .0
            .split(';')
            .next()
            .unwrap_or_default()
            .to_string();
        let absent = WindowId("window:hwnd=0x0000000000000001;pid=1".to_string());
        let recycled = WindowId(format!("{handle_field};pid=1"));
        let malformed = WindowId("frame-0204-not-an-identity".to_string());
        let start_error = |id: &WindowId| {
            WindowsWindowCaptureSession::start_diagnostic(
                id,
                FpsMode::AutoMatchSource,
                CaptureContext {
                    clock: Arc::new(MasterClock::default()),
                    cancellation: CaptureCancellation::default(),
                },
            )
            .err()
            .map(|error| error.code)
        };
        let absent_code = start_error(&absent);
        let recycled_code = start_error(&recycled);
        let malformed_code = start_error(&malformed);
        report.push_str(&format!(
            "start_validation: absent_handle={absent_code:?} reused_handle_other_process={recycled_code:?} malformed_identity={malformed_code:?}\n"
        ));
        checks.check(
            absent_code == Some(ErrorCode::SourceLost),
            "a handle that no longer exists is refused with SourceLost before capture starts",
        );
        checks.check(
            recycled_code == Some(ErrorCode::SourceLost),
            "a handle that now belongs to another process is refused with SourceLost",
        );
        checks.check(
            malformed_code == Some(ErrorCode::InvalidRequest),
            "a malformed window identity is refused as an invalid request",
        );

        // A visible resize recreates only the WGC pool. The same output pool,
        // timestamp epoch and source identity survive while content fits its canvas.
        let mut resize_session = WindowsWindowCaptureSession::start_diagnostic(
            &fixture_window.id,
            FpsMode::AutoMatchSource,
            CaptureContext {
                clock: Arc::new(MasterClock::default()),
                cancellation: CaptureCancellation::default(),
            },
        )?;
        let before_resize = wait_for_frame(&mut resize_session, FIRST_FRAME_BOUND)?;
        let before_canvas = (before_resize.width, before_resize.height);
        let before_timestamp = before_resize.timestamp;
        drop(before_resize);
        if !fixture.resize(660, 402) {
            return Err(internal_error(
                "The fixture window could not be resized.".into(),
            ));
        }
        let resized = observe(&mut resize_session, "resized", Duration::from_secs(3));
        let after_resize = wait_for_frame(&mut resize_session, FIRST_FRAME_BOUND)?;
        let after_canvas = (after_resize.width, after_resize.height);
        report.push_str(&format!(
            "resize: frames={} elapsed={} ms error={:?} same_canvas={}\n",
            resized.frames,
            resized.elapsed.as_millis(),
            resized.terminal,
            before_canvas == after_canvas
        ));
        checks.check(
            resized.terminal.is_none()
                && resized.frames > 0
                && before_canvas == after_canvas
                && after_resize.timestamp > before_timestamp,
            "visible resize continues on its original canvas and native timeline",
        );
        drop(after_resize);
        let _ = resize_session.stop();
        if !fixture.resize(640, 402) {
            return Err(internal_error(
                "The fixture window could not be restored.".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));

        // Exact child selection is independent of the compact titled-window list.
        // Its pixels come from the verified parent's WGC surface, never the desktop.
        let child = fixture.child()?;
        let child_info = frame_windows::validate_window_selection(&child.id())?;
        checks.check(
            child_info.title.contains("parent window capture"),
            "child selection explicitly identifies parent-window capture",
        );
        let mut child_session = WindowsWindowCaptureSession::start_diagnostic(
            &child_info.id,
            FpsMode::AutoMatchSource,
            CaptureContext {
                clock: Arc::new(MasterClock::default()),
                cancellation: CaptureCancellation::default(),
            },
        )?;
        let first_child = wait_for_frame(&mut child_session, FIRST_FRAME_BOUND)?;
        let child_canvas = (first_child.width, first_child.height);
        drop(first_child);
        child.resize(180, 130)?;
        let child_phase = observe(&mut child_session, "child resize", STATE_WINDOW);
        let resized_child = wait_for_frame(&mut child_session, FIRST_FRAME_BOUND)?;
        checks.check(
            child_phase.terminal.is_none()
                && child_phase.frames > 0
                && (resized_child.width, resized_child.height) == child_canvas,
            "child resize continues capture on its original canvas",
        );
        drop(resized_child);
        child_session.stop()?;
        drop(child);

        let popup = fixture.popup()?;
        let popup_info = frame_windows::validate_window_selection(&popup.id())?;
        let mut popup_session = WindowsWindowCaptureSession::start_diagnostic(
            &popup_info.id,
            FpsMode::AutoMatchSource,
            CaptureContext {
                clock: Arc::new(MasterClock::default()),
                cancellation: CaptureCancellation::default(),
            },
        )?;
        let popup_frame = wait_for_frame(&mut popup_session, FIRST_FRAME_BOUND)?;
        checks.check(
            popup_frame.width > 0 && popup_frame.height > 0,
            "an untitled tool popup is selectable and captured through its own WGC item",
        );
        drop(popup_frame);
        popup_session.stop()?;
        drop(popup);

        let mut session = WindowsWindowCaptureSession::start_diagnostic(
            &fixture_window.id,
            FpsMode::AutoMatchSource,
            CaptureContext {
                clock: Arc::new(MasterClock::default()),
                cancellation: CaptureCancellation::default(),
            },
        )?;
        report.push_str(&format!("started: window_id={}\n", session.window_id().0));

        // 1. visible. The first frame is released immediately: this run must not retain a
        // lease except in the one phase that deliberately does.
        let first = wait_for_frame(&mut session, FIRST_FRAME_BOUND);
        checks.check(
            first.is_ok(),
            "the visible fixture window produces a captured frame",
        );
        drop(first);
        let visible = observe(&mut session, "visible", STATE_WINDOW);
        note(&mut report, visible.describe());
        checks.check(
            visible.frames > 0 && visible.terminal.is_none(),
            "capture is live while the fixture is visible",
        );

        // 2. minimized: Windows.Graphics.Capture stops composing an iconified window. The
        // frame pool may still hand over the one frame it already held at the original
        // geometry, so the bound is "at most one", not zero; what must not happen is a
        // stream of frames, a rescaled frame, or a false failure.
        fixture.minimize();
        let minimized = observe(&mut session, "minimized", STATE_WINDOW);
        note(&mut report, minimized.describe());
        checks.check(
            minimized.frames <= u64::from(frame_windows::WINDOW_FRAME_POOL_BUFFERS as u32)
                && minimized.terminal.is_none(),
            "a minimized window publishes no new frame and reports no false success",
        );

        // 3. restored.
        fixture.restore();
        let restored = observe(&mut session, "restored", STATE_WINDOW);
        note(&mut report, restored.describe());
        checks.check(
            restored.frames > 0,
            "capture resumes when the fixture is restored",
        );

        // 4. hidden.
        fixture.hide();
        let hidden = observe(&mut session, "hidden", STATE_WINDOW);
        note(&mut report, hidden.describe());
        checks.check(
            hidden.frames <= u64::from(frame_windows::WINDOW_FRAME_POOL_BUFFERS as u32)
                && hidden.terminal.is_none(),
            "a hidden window publishes no new frame and reports no false success",
        );

        // 5. shown again.
        fixture.show();
        let shown = observe(&mut session, "shown", STATE_WINDOW);
        note(&mut report, shown.describe());
        checks.check(
            shown.frames > 0,
            "capture resumes when the fixture is shown again",
        );
        let after_states = session_counters(&session);
        report.push_str(&format!("state_phases: {after_states}\n"));
        checks.check(
            after_states.consumer_owned == 0,
            "every lease taken by the state phases was returned",
        );

        // 6. destroyed while the session is live, with one lease held across the close.
        let held = wait_for_frame(&mut session, FIRST_FRAME_BOUND)?;
        fixture.destroy();
        let closure = wait_for_terminal(&mut session, CLOSURE_BOUND);
        report.push_str(&format!(
            "closure: reported_after={} ms error={:?}\n",
            closure.0.as_millis(),
            closure.1
        ));
        checks.check(
            closure.1 == Some(ErrorCode::SourceLost),
            "a destroyed window is reported as an explicit SourceLost",
        );

        // 7. cleanup with a lease held across the close.
        report.push_str(&format!(
            "leases_through_stop: {}\n",
            session_counters(&session)
        ));
        let stop_result = session.stop();
        checks.check(
            stop_result
                .as_ref()
                .err()
                .is_some_and(|error| error.code == ErrorCode::SourceLost),
            "stop reports the same sticky SourceLost the closure produced",
        );
        let diagnostics = session.diagnostics()?;
        let after_stop = session_counters(&session);
        report.push_str(&format!("post_stop: {after_stop}\n"));
        checks.check(
            after_stop.consumer_owned == 1,
            "a lease held across the close stays valid and owns its slot",
        );
        drop(held);
        let after_release = session_counters(&session);
        report.push_str(&format!("post_release: {after_release}\n"));
        checks.check(
            after_release.slot_free == 4,
            "every slot returns to the pool once the held lease is released",
        );
        checks.check(
            after_release.payload_owners_live == 0,
            "the GPU payload is destroyed once the last lease is gone",
        );
        checks.check(
            after_release.queue_depth == 0 && after_release.ready_queued == 0,
            "no frame is left queued after the joined stop",
        );
        report.push('\n');
        report.push_str(&diagnostics);

        report.push_str("\nchecks:\n");
        for passed in &checks.passed {
            report.push_str(&format!("  PASS {passed}\n"));
        }
        for failed in &checks.failed {
            report.push_str(&format!("  FAIL {failed}\n"));
        }
        report.push_str(&format!(
            "summary: {} passed, {} failed\n",
            checks.passed.len(),
            checks.failed.len()
        ));
        Ok((report, checks.failed.len()))
    }

    /// Records one report line and prints it immediately.
    ///
    /// Printing as the run proceeds is what makes a crash or a hang locatable instead of
    /// leaving an empty log behind.
    fn note(report: &mut String, line: String) {
        println!("{line}");
        report.push_str(&line);
        report.push('\n');
    }

    /// Counts the frames that arrive within `window`, and the first terminal error.
    fn observe(
        session: &mut WindowsWindowCaptureSession,
        name: &'static str,
        window: Duration,
    ) -> Phase {
        let started = Instant::now();
        let deadline = started + window;
        let mut frames = 0u64;
        let mut terminal = None;
        while Instant::now() < deadline {
            match session.try_frame() {
                Ok(Some(frame)) => {
                    frames += 1;
                    drop(frame);
                }
                Ok(None) => std::thread::sleep(IDLE_PAUSE),
                Err(error) => {
                    terminal = Some(error.code);
                    break;
                }
            }
        }
        Phase {
            name,
            frames,
            elapsed: started.elapsed(),
            terminal,
        }
    }

    fn wait_for_frame(
        session: &mut WindowsWindowCaptureSession,
        bound: Duration,
    ) -> Result<CapturedFrame<WindowsSurface>, RecorderError> {
        let deadline = Instant::now() + bound;
        loop {
            match session.try_frame()? {
                Some(frame) => return Ok(frame),
                None => {
                    if Instant::now() >= deadline {
                        return Err(internal_error(format!(
                            "The fixture window produced no captured frame within {} ms.",
                            bound.as_millis()
                        )));
                    }
                    std::thread::sleep(IDLE_PAUSE);
                }
            }
        }
    }

    /// Waits for the session's terminal error and reports how long that took.
    fn wait_for_terminal(
        session: &mut WindowsWindowCaptureSession,
        bound: Duration,
    ) -> (Duration, Option<ErrorCode>) {
        let started = Instant::now();
        let deadline = started + bound;
        while Instant::now() < deadline {
            match session.try_frame() {
                Ok(_) => std::thread::sleep(IDLE_PAUSE),
                Err(error) => return (started.elapsed(), Some(error.code)),
            }
        }
        (started.elapsed(), None)
    }

    /// The bounded pool, queue and payload facts this example reads back.
    ///
    /// They come out of the session's own diagnostics report, because that is the
    /// session's public evidence surface: a caller has no access to the private worker
    /// counters, and this example deliberately uses only the public API.
    #[derive(Default)]
    struct Pooled {
        slot_free: u64,
        ready_queued: u64,
        consumer_owned: u64,
        queue_depth: u64,
        payload_owners_live: i64,
        unpublishable: u64,
    }

    impl std::fmt::Display for Pooled {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "slot_free={} ready_queued={} consumer_owned={} queue_depth={} payload_owners_live={} unpublishable={}",
                self.slot_free,
                self.ready_queued,
                self.consumer_owned,
                self.queue_depth,
                self.payload_owners_live,
                self.unpublishable
            )
        }
    }

    fn session_counters(session: &WindowsWindowCaptureSession) -> Pooled {
        let Ok(report) = session.diagnostics() else {
            return Pooled::default();
        };
        let mut pooled = Pooled::default();
        for line in report.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("slots:") {
                pooled.slot_free = field(trimmed, "free");
                pooled.ready_queued = field(trimmed, "ready_queued");
                pooled.consumer_owned = field(trimmed, "consumer_owned");
            }
            if trimmed.starts_with("queue:") {
                pooled.queue_depth = field(trimmed, "depth");
            }
            if trimmed.starts_with("payload:") {
                pooled.payload_owners_live = field(trimmed, "owners_live") as i64;
            }
            if trimmed.starts_with("unpublishable_updates:") {
                let text = trimmed
                    .trim_start_matches("unpublishable_updates:")
                    .trim_start();
                pooled.unpublishable = text
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            }
        }
        pooled
    }

    /// Reads one `name=value` field out of a report line.
    fn field(line: &str, name: &str) -> u64 {
        for token in line.split_whitespace() {
            if let Some(rest) = token.strip_prefix(&format!("{name}=")) {
                return rest.parse().unwrap_or(0);
            }
        }
        0
    }

    /// The locally created fixture window: a real top-level window with its own class,
    /// message pump and repainting timer.
    mod fixture {
        use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
        use std::time::{Duration, Instant};

        use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
        use windows_sys::Win32::Graphics::Gdi::{
            BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect,
            PAINTSTRUCT,
        };
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
            DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW,
            IsWindow, PostMessageW, PostThreadMessageW, RegisterClassW, SW_HIDE, SW_MINIMIZE,
            SW_RESTORE, SW_SHOW, SWP_NOZORDER, SetTimer, SetWindowLongPtrW, SetWindowPos,
            ShowWindow, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_PAINT, WM_QUIT, WM_TIMER,
            WNDCLASSW, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        };

        /// The handle the pump thread publishes once the window exists.
        static HANDLE: AtomicIsize = AtomicIsize::new(0);
        /// The pump thread's id, so the pump can be asked to exit.
        static THREAD_ID: AtomicU32 = AtomicU32::new(0);

        /// How many times the fixture has repainted; the colour changes with it, so the
        /// window keeps producing distinct compositions.
        struct PaintState {
            paints: u64,
        }

        pub(super) struct Fixture {
            handle: isize,
            thread_id: u32,
            thread: Option<std::thread::JoinHandle<()>>,
        }

        pub(super) struct Subwindow(HWND);
        impl Subwindow {
            pub(super) fn id(&self) -> frame_contracts::WindowId {
                frame_contracts::WindowId(format!(
                    "window:hwnd=0x{:016X};pid={}",
                    self.0 as usize,
                    std::process::id()
                ))
            }
            pub(super) fn resize(&self, width: i32, height: i32) -> Result<(), RecorderError> {
                if unsafe {
                    SetWindowPos(
                        self.0,
                        std::ptr::null_mut(),
                        30,
                        45,
                        width,
                        height,
                        SWP_NOZORDER,
                    )
                } == 0
                {
                    return Err(internal_error("Fixture child resize failed.".into()));
                }
                Ok(())
            }
        }
        impl Drop for Subwindow {
            fn drop(&mut self) {
                unsafe { DestroyWindow(self.0) };
            }
        }

        unsafe extern "system" fn window_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe {
                match message {
                    WM_PAINT => {
                        let mut paint = PAINTSTRUCT::default();
                        let device = BeginPaint(hwnd, &mut paint);
                        let mut client = RECT::default();
                        GetClientRect(hwnd, &mut client);
                        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PaintState;
                        let mut color: u32 = 0x0020_0000;
                        if !state.is_null() {
                            (*state).paints = (*state).paints.wrapping_add(1);
                            color =
                                (((*state).paints as u32).wrapping_mul(40) & 0xFF) | 0x0020_0000;
                        }
                        let brush = CreateSolidBrush(color);
                        FillRect(device, &client, brush);
                        let _ = DeleteObject(brush);
                        let _ = EndPaint(hwnd, &paint);
                        0
                    }
                    WM_TIMER => {
                        let _ = InvalidateRect(hwnd, std::ptr::null(), 0);
                        0
                    }
                    WM_CLOSE => {
                        let _ = DestroyWindow(hwnd);
                        0
                    }
                    WM_DESTROY => 0,
                    _ => DefWindowProcW(hwnd, message, wparam, lparam),
                }
            }
        }

        impl Fixture {
            pub(super) fn child(&self) -> Result<Subwindow, RecorderError> {
                self.subwindow(windows_sys::Win32::UI::WindowsAndMessaging::WS_CHILD, 0)
            }
            pub(super) fn popup(&self) -> Result<Subwindow, RecorderError> {
                self.subwindow(
                    windows_sys::Win32::UI::WindowsAndMessaging::WS_POPUP,
                    windows_sys::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW,
                )
            }
            fn subwindow(&self, style: u32, ex_style: u32) -> Result<Subwindow, RecorderError> {
                let hwnd = unsafe {
                    CreateWindowExW(
                        ex_style,
                        windows_sys::w!("STATIC"),
                        windows_sys::w!(""),
                        style | WS_VISIBLE,
                        30,
                        45,
                        200,
                        100,
                        self.handle(),
                        std::ptr::null_mut(),
                        GetModuleHandleW(std::ptr::null()),
                        std::ptr::null(),
                    )
                };
                if hwnd.is_null() {
                    return Err(internal_error("Fixture subwindow creation failed.".into()));
                }
                unsafe {
                    ShowWindow(hwnd, SW_SHOW);
                    ShowWindow(hwnd, SW_SHOW);
                }
                Ok(Subwindow(hwnd))
            }
            /// Creates the fixture window on its own thread and waits for it to exist.
            pub(super) fn spawn() -> Result<Self, RecorderError> {
                HANDLE.store(0, Ordering::Release);
                let thread = std::thread::Builder::new()
                    .name("frame-0204-fixture".to_string())
                    .spawn(|| unsafe {
                        THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);
                        let module = GetModuleHandleW(std::ptr::null());
                        let class_name: Vec<u16> =
                            "Frame0204FixtureWindow\0".encode_utf16().collect();
                        let class = WNDCLASSW {
                            style: CS_HREDRAW | CS_VREDRAW,
                            lpfnWndProc: Some(window_proc),
                            hInstance: module,
                            lpszClassName: class_name.as_ptr(),
                            ..Default::default()
                        };
                        if RegisterClassW(&class) == 0 {
                            return;
                        }
                        let title: Vec<u16> = format!("{}\0", super::FIXTURE_TITLE)
                            .encode_utf16()
                            .collect();
                        let hwnd = CreateWindowExW(
                            WS_EX_APPWINDOW,
                            class_name.as_ptr(),
                            title.as_ptr(),
                            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                            80,
                            80,
                            640,
                            // At this fixture's 150% desktop scale the DWM capture
                            // height is 592, so the 4:2:0 recording probe can encode it.
                            402,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            module,
                            std::ptr::null(),
                        );
                        if hwnd.is_null() {
                            return;
                        }
                        // A diagnostic launched with a hidden console still needs its
                        // own capture window visible. The first ShowWindow may consume
                        // the process startup hint, so apply the fixture state again.
                        ShowWindow(hwnd, SW_SHOW);
                        ShowWindow(hwnd, SW_SHOW);
                        let state = Box::into_raw(Box::new(PaintState { paints: 0 }));
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                        // A repainting timer: the fixture keeps producing distinct
                        // compositions, which is what makes it a live capture source.
                        SetTimer(hwnd, 1, 10, None);
                        HANDLE.store(hwnd as isize, Ordering::Release);
                        let mut message = std::mem::zeroed();
                        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                        // The pump is finished. The window is destroyed on this same
                        // thread before the paint state is freed, so no pending paint can
                        // reach a freed state; `WM_CLOSE` may already have destroyed it.
                        if IsWindow(hwnd) != 0 {
                            let _ = DestroyWindow(hwnd);
                        }
                        let _ = Box::from_raw(state);
                    })
                    .map_err(|error| {
                        internal_error(format!(
                            "The fixture window thread could not be created: {error}"
                        ))
                    })?;

                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let handle = HANDLE.load(Ordering::Acquire);
                    if handle != 0 {
                        return Ok(Self {
                            handle,
                            thread_id: THREAD_ID.load(Ordering::Acquire),
                            thread: Some(thread),
                        });
                    }
                    if Instant::now() >= deadline {
                        return Err(internal_error(
                            "The fixture window was not created within 5000 ms.".to_string(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }

            pub(super) fn thread_id_for_diagnostics(&self) -> u32 {
                self.thread_id
            }

            fn handle(&self) -> HWND {
                self.handle as HWND
            }

            pub(super) fn minimize(&self) {
                unsafe { ShowWindow(self.handle(), SW_MINIMIZE) };
            }

            pub(super) fn restore(&self) {
                unsafe { ShowWindow(self.handle(), SW_RESTORE) };
            }

            pub(super) fn hide(&self) {
                unsafe { ShowWindow(self.handle(), SW_HIDE) };
            }

            pub(super) fn show(&self) {
                unsafe { ShowWindow(self.handle(), SW_SHOW) };
            }

            pub(super) fn resize(&self, width: i32, height: i32) -> bool {
                unsafe {
                    SetWindowPos(
                        self.handle(),
                        std::ptr::null_mut(),
                        80,
                        80,
                        width,
                        height,
                        SWP_NOZORDER,
                    ) != 0
                }
            }

            /// Closes the fixture window the way a user would: a `WM_CLOSE` to its own
            /// pump thread, which destroys it.
            pub(super) fn destroy(&self) {
                unsafe { PostMessageW(self.handle(), WM_CLOSE, 0, 0) };
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if unsafe { IsWindow(self.handle()) } == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                // Bounded, explicit teardown of the fixture's own thread; no other
                // process's window is ever involved.
                let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }

        fn internal_error(message: String) -> RecorderError {
            RecorderError {
                code: ErrorCode::Internal,
                message,
                recovery: RecoveryStatus::Unknown,
                action: "Do not treat this run as evidence; report the failure.".to_string(),
            }
        }
    }
}
