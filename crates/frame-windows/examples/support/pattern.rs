#[cfg(target_os = "windows")]
mod pattern {
    #![allow(dead_code)] // Shared by region and live performance diagnostics.
    use std::mem::zeroed;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::mpsc::{SyncSender, sync_channel};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
    use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HBRUSH, HDC, HGDIOBJ,
        InvalidateRect, PAINTSTRUCT, UpdateWindow,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetClientRect, GetWindowDisplayAffinity, HWND_TOPMOST, MSG, PM_REMOVE, PeekMessageW,
        PostMessageW, PostQuitMessage, RegisterClassW, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        SetWindowDisplayAffinity, SetWindowPos, TranslateMessage, UnregisterClassW,
        WDA_EXCLUDEFROMCAPTURE, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_QUIT, WNDCLASSW,
        WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    const CLASS_NAME: &str = "FrameRegionPatternWindow";
    /// Bound on the ready handshake of the window thread.
    const READY_WATCHDOG: Duration = Duration::from_secs(5);
    /// The moving block that keeps the desktop changing: its side, its step and its period.
    const MARKER_SIZE: i32 = 64;
    const MARKER_STEP: i32 = 8;
    const MARKER_PERIOD: Duration = Duration::from_millis(8);
    /// The block's colour: white, which is not in the recorded palette's band either.
    const MARKER_COLOR: [u8; 3] = [255, 255, 255];

    /// Left edge of the moving block, in client (display-local) pixels. The window
    /// procedure and the animation step run on the same thread, and an atomic keeps the
    /// static state explicit rather than hidden in a thread-local.
    static MARKER_LEFT: AtomicI32 = AtomicI32::new(0);
    static PREVIEW: AtomicBool = AtomicBool::new(false);
    static TIMING: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
    static TIMING_ACTIVE: AtomicBool = AtomicBool::new(false);

    /// The deterministic pattern the window paints, in display-local physical pixels.
    #[derive(Clone)]
    pub(super) struct PatternSpec {
        pub(super) stripe_width: u32,
        /// Vertical bars, indexed by `x / stripe_width % palette.len()`.
        pub(super) palette: [[u8; 3]; 6],
        /// The reference colour from [`PatternSpec::band_height`] downwards.
        pub(super) band_color: [u8; 3],
        /// The display-local row where the bars end and the band begins.
        pub(super) band_height: u32,
    }

    /// The specification the window procedure paints. One pattern window exists per process,
    /// so a process-wide cell is all the state it needs.
    static PATTERN: OnceLock<PatternSpec> = OnceLock::new();

    /// One running pattern window, owned by the thread that created it.
    pub(super) struct PatternWindow {
        handle: isize,
        pub(super) child: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl PatternWindow {
        /// Creates the window on its own thread and waits until it has painted once.
        pub(super) fn show(
            spec: PatternSpec,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        ) -> Result<Self, RecorderError> {
            Self::show_with_preview(spec, x, y, width, height, false)
        }

        pub(super) fn show_with_preview(
            spec: PatternSpec,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            preview: bool,
        ) -> Result<Self, RecorderError> {
            PREVIEW.store(preview, Ordering::Relaxed);
            PATTERN.set(spec).map_err(|_| {
                pattern_error("The pattern specification was already set for this process.".into())
            })?;
            let (ready_tx, ready_rx) = sync_channel::<Result<(isize, isize), RecorderError>>(1);
            let thread = std::thread::Builder::new()
                .name("frame-region-pattern".to_string())
                .spawn(move || create_and_pump(x, y, width, height, ready_tx))
                .map_err(|error| {
                    pattern_error(format!(
                        "The pattern window thread could not be started: {error}"
                    ))
                })?;
            match ready_rx.recv_timeout(READY_WATCHDOG) {
                Ok(Ok((handle, child))) => Ok(Self {
                    handle,
                    child,
                    thread: Some(thread),
                }),
                Ok(Err(error)) => {
                    let _ = thread.join();
                    Err(error)
                }
                Err(_) => Err(pattern_error(format!(
                    "The pattern window did not become ready within {} ms.",
                    READY_WATCHDOG.as_millis()
                ))),
            }
        }

        pub(super) fn reset_timing(&self) {
            TIMING.lock().unwrap().clear();
            TIMING_ACTIVE.store(true, Ordering::Relaxed);
        }
        pub(super) fn timing(&self) -> Vec<(u64, u64)> {
            TIMING.lock().unwrap().clone()
        }

        /// Fails when the window thread already ended, so a probe never records the desktop
        /// behind a window that is no longer there.
        pub(super) fn check_alive(&self) -> Result<(), RecorderError> {
            if self
                .thread
                .as_ref()
                .is_some_and(|thread| thread.is_finished())
            {
                return Err(pattern_error(
                    "The pattern window thread ended before recording started.".to_string(),
                ));
            }
            Ok(())
        }
    }

    impl Drop for PatternWindow {
        fn drop(&mut self) {
            // The window is closed through its own queue, so its procedure runs on the thread
            // that created it. DestroyWindow cannot be called from another thread.
            // SAFETY: the handle is a live window owned by this value, and WM_CLOSE is
            // delivered to its own message loop.
            let _ = unsafe { PostMessageW(self.handle as HWND, WM_CLOSE, 0, 0) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn pattern_error(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::Internal,
            message,
            recovery: RecoveryStatus::NoRecording,
            action: "No recording was produced; report the observed window failure.".to_string(),
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// GDI `COLORREF` is `0x00BBGGRR`.
    fn colorref(rgb: [u8; 3]) -> COLORREF {
        u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16)
    }

    fn create_and_pump(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        ready: SyncSender<Result<(isize, isize), RecorderError>>,
    ) {
        match create(x, y, width, height) {
            Ok(handle) => {
                // A child in the source window would create capture feedback and
                // change the workload between off/on. Own an excluded top-level
                // preview host instead; API failure makes the benchmark fail.
                let (preview_host, child) = if PREVIEW.load(Ordering::Relaxed) {
                    match create_preview_host(handle as HWND, x, y) {
                        Ok(pair) => pair,
                        Err(error) => {
                            let _ = ready.send(Err(pattern_error(error)));
                            unsafe {
                                DestroyWindow(handle as HWND);
                            }
                            return;
                        }
                    }
                } else {
                    (std::ptr::null_mut(), 0)
                };
                if ready.send(Ok((handle, child))).is_err() {
                    unsafe {
                        if !preview_host.is_null() {
                            DestroyWindow(preview_host);
                        }
                        DestroyWindow(handle as HWND);
                    }
                    return;
                }
                pump(handle as HWND);
                unsafe {
                    if !preview_host.is_null() {
                        DestroyWindow(preview_host);
                    }
                }
                // SAFETY: the class was registered by this thread and its window is gone.
                unsafe { UnregisterClassW(wide(CLASS_NAME).as_ptr(), std::ptr::null_mut()) };
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        }
    }

    fn create_preview_host(owner: HWND, x: i32, y: i32) -> Result<(HWND, isize), String> {
        let class = wide("STATIC");
        let host = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP | WS_VISIBLE,
                x + 8,
                y + 8,
                320,
                180,
                owner,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if host.is_null() {
            return Err("Could not create diagnostic preview host.".into());
        }
        let result = (|| {
            let mut affinity = 0;
            if unsafe { SetWindowDisplayAffinity(host, WDA_EXCLUDEFROMCAPTURE) } == 0
                || unsafe { GetWindowDisplayAffinity(host, &mut affinity) } == 0
                || affinity != WDA_EXCLUDEFROMCAPTURE
            {
                return Err("Preview exclusion from capture could not be established.".into());
            }
            let child = frame_windows::gpu_preview::create_child_window(host as isize)?;
            frame_windows::gpu_preview::place_child_window(child, (0, 0, 320, 180), true)?;
            Ok((host, child))
        })();
        if result.is_err() {
            unsafe {
                DestroyWindow(host);
            }
        }
        result
    }

    fn create(x: i32, y: i32, width: u32, height: u32) -> Result<isize, RecorderError> {
        // The window is created in this thread's per-monitor context, so its rectangle is in
        // physical pixels and lines up with the canonical region space instead of being
        // virtualized for a DPI-unaware process.
        // SAFETY: the context constant is a documented predefined value and the call only
        // changes this thread's own DPI context.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // SAFETY: a null module name asks for this process's own module, which is the
        // instance the window class belongs to.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if instance.is_null() {
            return Err(pattern_error(
                "GetModuleHandleW returned no module handle for the pattern window.".to_string(),
            ));
        }
        let class = wide(CLASS_NAME);
        let window_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            // The whole client area is painted in WM_PAINT and WM_ERASEBKGND answers 1, so no
            // background brush is needed and no flash of an unpainted window is introduced.
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        // SAFETY: the class structure is fully initialized and its string pointers live until
        // the class is unregistered after the window is destroyed.
        let atom = unsafe { RegisterClassW(&window_class) };
        if atom == 0 {
            return Err(pattern_error(
                "RegisterClassW failed for the pattern window class.".to_string(),
            ));
        }
        let title = wide("Frame region capture pattern");
        // SAFETY: the class name is registered, the style is a borderless topmost popup, the
        // size is in this thread's physical pixels, and a null parent and menu are valid.
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(pattern_error(
                "CreateWindowExW failed for the pattern window.".to_string(),
            ));
        }
        // SAFETY: the handle is a live window owned by this thread; the call only raises and
        // shows it without activating it, and the first paint below is synchronous.
        unsafe {
            SetWindowPos(
                handle,
                HWND_TOPMOST,
                x,
                y,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = UpdateWindow(handle);
        }
        Ok(handle as isize)
    }

    /// Pumps this thread's messages and advances the moving block, so the composited desktop
    /// keeps changing while the recorded pattern stays exactly the same.
    fn pump(handle: HWND) {
        let mut message: MSG = unsafe { zeroed() };
        let mut last_step = Instant::now();
        loop {
            // A null window filter is deliberate: WM_QUIT is a thread message and would not
            // be returned by a window-filtered peek, and this thread owns only this window.
            loop {
                // SAFETY: `message` is a correctly sized out-parameter owned by this thread.
                let pending =
                    unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
                if pending == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    return;
                }
                // SAFETY: `message` was filled by the successful PeekMessageW above.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if last_step.elapsed() >= MARKER_PERIOD {
                let now = Instant::now();
                let interval = now.duration_since(last_step).as_nanos() as u64;
                last_step = now;
                advance_marker(handle);
                let work = now.elapsed().as_nanos() as u64;
                if TIMING_ACTIVE.load(Ordering::Relaxed)
                    && let Ok(mut samples) = TIMING.lock()
                    && samples.len() < 8192
                {
                    samples.push((interval, work));
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Moves the block one step and repaints only the two rectangles it left and entered.
    fn advance_marker(handle: HWND) {
        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: the window is alive on this thread and the rectangle is a valid
        // out-parameter.
        let _ = unsafe { GetClientRect(handle, &mut client) };
        let previous = MARKER_LEFT.load(Ordering::Relaxed);
        let mut next = previous + MARKER_STEP;
        if next + MARKER_SIZE > client.right {
            next = 0;
        }
        MARKER_LEFT.store(next, Ordering::Relaxed);
        let top = client.bottom - MARKER_SIZE - 8;
        let dirty = RECT {
            left: previous.min(next),
            top,
            right: (previous + MARKER_SIZE).max(next + MARKER_SIZE),
            bottom: top + MARKER_SIZE,
        };
        // SAFETY: the window is alive on this thread; `InvalidateRect` only marks the
        // rectangle dirty and `UpdateWindow` delivers the resulting paint synchronously.
        unsafe {
            let _ = InvalidateRect(handle, &dirty, 0);
            let _ = UpdateWindow(handle);
        }
    }

    /// Paints the pattern: tiled vertical bars, the reference band and the moving block.
    ///
    /// SAFETY: the caller passes the live window of this thread.
    unsafe fn paint(handle: HWND) {
        let Some(spec) = PATTERN.get() else {
            return;
        };
        let mut paint: PAINTSTRUCT = unsafe { zeroed() };
        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: the window is alive on this thread and the structure is a valid
        // out-parameter for BeginPaint.
        let device = unsafe { BeginPaint(handle, &mut paint) };
        if device.is_null() {
            return;
        }
        // SAFETY: the window is alive on this thread and the rectangle is a valid
        // out-parameter.
        let _ = unsafe { GetClientRect(handle, &mut client) };
        let stripe = i32::try_from(spec.stripe_width).unwrap_or(1).max(1);
        let band_top = i32::try_from(spec.band_height).unwrap_or(client.bottom);
        let bar_bottom = band_top.min(client.bottom);
        let mut left = client.left;
        let mut index = 0usize;
        while left < client.right {
            let right = (left + stripe).min(client.right);
            let bar = RECT {
                left,
                top: client.top,
                right,
                bottom: bar_bottom,
            };
            fill(device, &bar, spec.palette[index % spec.palette.len()]);
            left = right;
            index += 1;
        }
        if band_top < client.bottom {
            let band = RECT {
                left: client.left,
                top: band_top,
                right: client.right,
                bottom: client.bottom,
            };
            fill(device, &band, spec.band_color);
        }
        // The block sits at the very bottom of the display, far below every rectangle this
        // probe records; it exists only to keep the desktop presenting new images.
        let marker_left = MARKER_LEFT.load(Ordering::Relaxed);
        let marker_top = client.bottom - MARKER_SIZE - 8;
        let marker = RECT {
            left: marker_left,
            top: marker_top,
            right: marker_left + MARKER_SIZE,
            bottom: marker_top + MARKER_SIZE,
        };
        fill(device, &marker, MARKER_COLOR);
        // SAFETY: matches the BeginPaint above on this thread.
        unsafe { EndPaint(handle, &paint) };
    }

    fn fill(device: HDC, rect: &RECT, rgb: [u8; 3]) {
        // SAFETY: the brush is created for this one fill and deleted immediately, and the
        // device context is the one BeginPaint returned on this thread.
        unsafe {
            let brush: HBRUSH = CreateSolidBrush(colorref(rgb));
            if brush.is_null() {
                return;
            }
            FillRect(device, rect, brush);
            let _ = DeleteObject(brush as HGDIOBJ);
        }
    }

    unsafe extern "system" fn window_proc(
        handle: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_PAINT => {
                // SAFETY: the message was delivered to this thread's live window.
                unsafe { paint(handle) };
                0
            }
            // The whole client area is painted explicitly, so nothing is erased here.
            WM_ERASEBKGND => 1,
            WM_CLOSE => {
                // SAFETY: the handle is the live window of this thread.
                unsafe { DestroyWindow(handle) };
                0
            }
            WM_DESTROY => {
                // SAFETY: posts this thread's quit message; no resources are involved.
                unsafe { PostQuitMessage(0) };
                0
            }
            // SAFETY: every other message is handled by the default procedure.
            _ => unsafe { DefWindowProcW(handle, message, wparam, lparam) },
        }
    }
}
