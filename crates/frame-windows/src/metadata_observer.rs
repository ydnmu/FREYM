//! Source-scoped mouse metadata on a dedicated message-loop thread. No keyboard,
//! title, clipboard, application history or input injection. Callbacks never wait
//! for capture, disk or a contended metadata queue. Geometry is sampled at 10 Hz.
use frame_contracts::CaptureTarget;
use frame_metadata::{Button, Capabilities, EventIngress, EventKind, Geometry, Support};
use std::{
    cell::RefCell,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MONITORINFO, MONITORINFOEXW},
    UI::WindowsAndMessaging::*,
};

pub fn interaction_capabilities(target: &CaptureTarget) -> Capabilities {
    Capabilities {
        cursor: Support::Sampled,
        buttons: Support::Native,
        scroll: Support::Native,
        geometry: Support::Sampled,
        source_focus: if matches!(target, CaptureTarget::Window(_)) {
            Support::Sampled
        } else {
            Support::Unsupported
        },
    }
}
enum Source {
    Window(crate::window_discovery::WindowIdentity),
    Display {
        monitor: isize,
        adapter: String,
        region: Option<frame_contracts::Region>,
    },
}
struct Observation {
    ingress: EventIngress,
    bounds: Option<Geometry>,
    active: bool,
    focus: Option<bool>,
    cursor: Option<(u32, u32)>,
    moved: Instant,
    window: Option<(isize, u32)>,
}
thread_local! { static OBSERVATION: RefCell<Option<Observation>> = const { RefCell::new(None) }; }
pub struct WindowsInteractionObserver {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}
impl WindowsInteractionObserver {
    pub fn start(target: CaptureTarget, ingress: EventIngress) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("freym-interaction".into())
            .spawn(move || {
                let source = match resolve(&target) {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return Err(error);
                    }
                };
                // Per-monitor cursor packet coordinates and sampled geometry must agree.
                let result = crate::display::with_per_monitor_awareness(|| {
                    run(source, ingress, worker_stop, &ready).map_err(|error| {
                        frame_contracts::RecorderError {
                            code: frame_contracts::ErrorCode::Internal,
                            message: error.to_string(),
                            recovery: frame_contracts::RecoveryStatus::Unknown,
                            action: "Preserve local metadata.".into(),
                        }
                    })
                })
                .map_err(|error| io::Error::other(error.message));
                if let Err(error) = &result {
                    let _ = ready.try_send(Err(error.to_string()));
                }
                result
            })?;
        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                worker: Some(worker),
            }),
            result => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(io::Error::other(match result {
                    Ok(Err(error)) => error,
                    _ => "Interaction observer failed to start.".into(),
                }))
            }
        }
    }
    pub fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| io::Error::other("Interaction observer panicked."))?,
            None => Ok(()),
        }
    }
}
impl Drop for WindowsInteractionObserver {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn run(
    source: Source,
    ingress: EventIngress,
    stop: Arc<AtomicBool>,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> io::Result<()> {
    let window = match &source {
        Source::Window(identity) => Some((identity.handle(), identity.process_id())),
        _ => None,
    };
    OBSERVATION.with(|v| {
        *v.borrow_mut() = Some(Observation {
            ingress,
            bounds: None,
            active: false,
            focus: None,
            cursor: None,
            moved: Instant::now() - Duration::from_secs(1),
            window,
        })
    });
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), std::ptr::null_mut(), 0) };
    if hook.is_null() {
        OBSERVATION.with(|v| v.borrow_mut().take());
        return Err(io::Error::last_os_error());
    }
    refresh(&source);
    let _ = ready.send(Ok(()));
    let mut refreshed = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let mut message = MSG::default();
        for _ in 0..256 {
            if unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
                break;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if refreshed.elapsed() >= Duration::from_millis(100) {
            refresh(&source);
            refreshed = Instant::now();
        }
        thread::sleep(Duration::from_millis(2));
    }
    let result = if unsafe { UnhookWindowsHookEx(hook) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    OBSERVATION.with(|v| v.borrow_mut().take());
    result
}
fn resolve(target: &CaptureTarget) -> io::Result<Source> {
    let platform = |error: frame_contracts::RecorderError| io::Error::other(error.message);
    match target {
        CaptureTarget::Window(id) => Ok(Source::Window(
            crate::window_discovery::WindowIdentity::parse(&id.0).map_err(platform)?,
        )),
        CaptureTarget::Display(id)
        | CaptureTarget::Region(frame_contracts::Region { display_id: id, .. }) => {
            let output = crate::display::capture_output(&id.0).map_err(platform)?;
            Ok(Source::Display {
                monitor: output.monitor_handle,
                adapter: output.adapter_name,
                region: if let CaptureTarget::Region(r) = target {
                    Some(r.clone())
                } else {
                    None
                },
            })
        }
        _ => Err(io::Error::other(
            "Metadata needs an explicit display, region or window.",
        )),
    }
}
fn geometry(source: &Source) -> Option<(Geometry, bool)> {
    match source {
        Source::Window(identity) => {
            let state = identity.verify().ok()?;
            if !state.visible || state.minimized {
                return None;
            }
            let root = identity.capture_root().ok()?;
            let root_state = root.verify().ok()?;
            if !root_state.visible || root_state.minimized {
                return None;
            }
            let hwnd = root.handle() as *mut std::ffi::c_void;
            let rect = identity.frame_bounds()?;
            // Only compare the foreground handle to the selected source. Never
            // resolve or persist the unrelated foreground window's properties.
            Some((from_rect(rect)?, unsafe { GetForegroundWindow() } == hwnd))
        }
        Source::Display {
            monitor,
            adapter,
            region,
        } => {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(*monitor as _, &mut info as *mut _ as *mut MONITORINFO) }
                == 0
            {
                return None;
            }
            let device = String::from_utf16_lossy(
                &info.szDevice[..info
                    .szDevice
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(info.szDevice.len())],
            );
            if !device.eq_ignore_ascii_case(adapter) {
                return None;
            }
            let display = from_rect(info.monitorInfo.rcMonitor)?;
            if let Some(region) = region {
                // Region is display-relative physical pixels, same as GPU crop.
                let bounds = Geometry {
                    x: display.x.checked_add(region.x)?,
                    y: display.y.checked_add(region.y)?,
                    width: region.width,
                    height: region.height,
                };
                display.relative(bounds.x, bounds.y)?;
                if i64::from(bounds.x) + i64::from(bounds.width)
                    > i64::from(display.x) + i64::from(display.width)
                    || i64::from(bounds.y) + i64::from(bounds.height)
                        > i64::from(display.y) + i64::from(display.height)
                {
                    return None;
                }
                Some((bounds, true))
            } else {
                Some((display, true))
            }
        }
    }
}
fn from_rect(rect: RECT) -> Option<Geometry> {
    let width = u32::try_from(i64::from(rect.right) - i64::from(rect.left)).ok()?;
    let height = u32::try_from(i64::from(rect.bottom) - i64::from(rect.top)).ok()?;
    (width > 0 && height > 0).then_some(Geometry {
        x: rect.left,
        y: rect.top,
        width,
        height,
    })
}
fn refresh(source: &Source) {
    let observed = geometry(source);
    OBSERVATION.with(|value| {
        let mut value = value.borrow_mut();
        let Some(state) = value.as_mut() else {
            return;
        };
        let Some((bounds, active)) = observed else {
            if state.bounds.is_some() {
                state.ingress.try_emit(EventKind::SourceUnavailable);
            }
            state.bounds = None;
            state.active = false;
            state.focus = None;
            state.cursor = None;
            return;
        };
        if state.bounds != Some(bounds) {
            if !state.ingress.try_emit(EventKind::Geometry { bounds }) {
                state.bounds = None;
                return;
            }
            state.bounds = Some(bounds);
            state.cursor = None;
        }
        if matches!(source, Source::Window(_)) && state.focus != Some(active) {
            if !state.ingress.try_emit(EventKind::Focus { active }) {
                state.active = false;
                return;
            }
            state.focus = Some(active);
        }
        state.active = active;
        let mut point = windows_sys::Win32::Foundation::POINT::default();
        if unsafe { GetCursorPos(&mut point) } != 0 {
            state.position(point.x, point.y);
        }
    });
}
impl Observation {
    fn source_active(&self) -> bool {
        if let Some((handle, process)) = self.window {
            let hwnd = handle as *mut std::ffi::c_void;
            if unsafe { GetForegroundWindow() } != hwnd {
                return false;
            }
            let mut actual = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut actual);
            }
            actual == process
        } else {
            self.active
        }
    }
    fn position(&mut self, x: i32, y: i32) {
        let point = self
            .bounds
            .filter(|_| self.source_active())
            .and_then(|b| b.relative(x, y));
        if point == self.cursor {
            return;
        }
        if let Some((x, y)) = point {
            if self.moved.elapsed() < Duration::from_nanos(16_666_667) {
                return;
            }
            if self.ingress.try_emit(EventKind::Cursor { x, y }) {
                self.cursor = point;
                self.moved = Instant::now();
            }
        } else if self.cursor.is_some() && self.ingress.try_emit(EventKind::CursorLeft) {
            self.cursor = None;
        }
    }
}
unsafe extern "system" fn mouse(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && data != 0 {
        let packet = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        OBSERVATION.with(|value| {
            let Ok(mut value) = value.try_borrow_mut() else {
                return;
            };
            let Some(state) = value.as_mut() else {
                return;
            };
            let message = message as u32;
            if message == WM_MOUSEMOVE {
                state.position(packet.pt.x, packet.pt.y);
                return;
            }
            let Some((x, y)) = state
                .bounds
                .filter(|_| state.source_active())
                .and_then(|b| b.relative(packet.pt.x, packet.pt.y))
            else {
                return;
            };
            let button = match message {
                WM_LBUTTONDOWN | WM_LBUTTONUP => Some(Button::Left),
                WM_RBUTTONDOWN | WM_RBUTTONUP => Some(Button::Right),
                WM_MBUTTONDOWN | WM_MBUTTONUP => Some(Button::Middle),
                WM_XBUTTONDOWN | WM_XBUTTONUP => match packet.mouseData >> 16 {
                    1 => Some(Button::Extra1),
                    2 => Some(Button::Extra2),
                    _ => None,
                },
                _ => None,
            };
            if let Some(button) = button {
                state.ingress.try_emit(EventKind::Button {
                    x,
                    y,
                    button,
                    pressed: matches!(
                        message,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                    ),
                });
            } else if matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
                state.ingress.try_emit(EventKind::Scroll {
                    x,
                    y,
                    horizontal: message == WM_MOUSEHWHEEL,
                    delta: i32::from((packet.mouseData >> 16) as i16),
                });
            }
        });
    }
    // Always forward; never interfere with the user's input.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, message, data) }
}
