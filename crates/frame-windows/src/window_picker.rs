//! A short-lived desktop picker. It returns a WindowInfo; it never starts capture.
//! Two ordinary Win32 windows intercept input and draw a white outline. No hooks,
//! screenshots, polling thread or persistent window list are needed.
#![allow(unsafe_code)]

use crate::window_discovery::{validate_selection, window_at_point};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus, WindowId, WindowInfo};
use std::{ptr::null_mut, sync::OnceLock};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        BLACK_BRUSH, CombineRgn, CreateRectRgn, DeleteObject, GetStockObject, InvalidateRect,
        RGN_DIFF, SetWindowRgn, WHITE_BRUSH,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        Input::KeyboardAndMouse::{MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_ESCAPE},
        WindowsAndMessaging::*,
    },
};

const INPUT_CLASS: *const u16 = windows_sys::w!("FREYM.WindowPicker.Input");
const FRAME_CLASS: *const u16 = windows_sys::w!("FREYM.WindowPicker.Frame");

fn picker_error(message: impl Into<String>) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Try again or choose a window from Source.".into(),
    }
}

struct Picker {
    outline: HWND,
    hover: Option<(WindowId, RECT)>,
    pressed: Option<WindowId>,
    selected: Option<WindowId>,
    finished: bool,
    failure: Option<RecorderError>,
}

impl Picker {
    fn update_hover(&mut self) -> Result<(), RecorderError> {
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) } == 0 {
            return Err(picker_error("Windows could not read the pointer position."));
        }
        let next = window_at_point(point.x, point.y);
        if self
            .hover
            .as_ref()
            .map(|(id, r)| (id, (r.left, r.top, r.right, r.bottom)))
            == next
                .as_ref()
                .map(|(id, r)| (id, (r.left, r.top, r.right, r.bottom)))
        {
            return Ok(());
        }
        unsafe { ShowWindow(self.outline, SW_HIDE) };
        self.hover = None;
        if let Some((_, bounds)) = &next {
            let width = bounds.right - bounds.left;
            let height = bounds.bottom - bounds.top;
            let outer = unsafe { CreateRectRgn(0, 0, width, height) };
            let inner = unsafe { CreateRectRgn(3, 3, (width - 3).max(3), (height - 3).max(3)) };
            if outer.is_null() || inner.is_null() {
                unsafe {
                    DeleteObject(outer);
                    DeleteObject(inner);
                }
                return Err(picker_error("Windows could not draw the window outline."));
            }
            let combined = unsafe { CombineRgn(outer, outer, inner, RGN_DIFF) };
            unsafe { DeleteObject(inner) };
            if combined == 0 || unsafe { SetWindowRgn(self.outline, outer, 1) } == 0 {
                unsafe { DeleteObject(outer) };
                return Err(picker_error("Windows could not shape the window outline."));
            }
            // Windows owns outer after SetWindowRgn succeeds.
            if unsafe {
                SetWindowPos(
                    self.outline,
                    HWND_TOPMOST,
                    bounds.left,
                    bounds.top,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
            } == 0
            {
                return Err(picker_error(
                    "Windows could not position the window outline.",
                ));
            }
            unsafe { InvalidateRect(self.outline, null_mut(), 1) };
        }
        self.hover = next;
        Ok(())
    }
}

unsafe extern "system" fn input_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Picker;
    if !ptr.is_null() {
        // The Picker stays boxed until both native windows are destroyed. Only this
        // thread dispatches input, and the outline uses DefWindowProcW, not this state.
        match msg {
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
            WM_MOUSEMOVE | WM_TIMER | WM_LBUTTONDOWN | WM_LBUTTONUP => {
                let picker = unsafe { &mut *ptr };
                if let Err(error) = picker.update_hover() {
                    picker.failure = Some(error);
                    picker.finished = true;
                } else if msg == WM_LBUTTONDOWN {
                    picker.pressed = picker.hover.as_ref().map(|(id, _)| id.clone());
                } else if msg == WM_LBUTTONUP {
                    // An opening click's release or a drag to another window does
                    // not select anything. The click is consumed by this overlay.
                    let current = picker.hover.as_ref().map(|(id, _)| id.clone());
                    if current.is_some() && picker.pressed == current {
                        picker.selected = current;
                        picker.finished = true;
                    }
                    picker.pressed = None;
                }
                return 0;
            }
            WM_HOTKEY if w == 1 => {
                unsafe {
                    (*ptr).finished = true;
                }
                return 0;
            }
            WM_KEYDOWN if w == VK_ESCAPE as usize => {
                // Escape
                unsafe {
                    (*ptr).finished = true;
                }
                return 0;
            }
            WM_CLOSE | WM_DISPLAYCHANGE => {
                unsafe {
                    (*ptr).finished = true;
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn register_classes() -> Result<(), RecorderError> {
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            let instance = unsafe { GetModuleHandleW(null_mut()) };
            for (name, proc, brush) in [
                (
                    INPUT_CLASS,
                    Some(input_proc as unsafe extern "system" fn(_, _, _, _) -> _),
                    BLACK_BRUSH,
                ),
                (
                    FRAME_CLASS,
                    Some(DefWindowProcW as unsafe extern "system" fn(_, _, _, _) -> _),
                    WHITE_BRUSH,
                ),
            ] {
                let class = WNDCLASSW {
                    lpfnWndProc: proc,
                    hInstance: instance,
                    hCursor: unsafe { LoadCursorW(null_mut(), IDC_CROSS) },
                    hbrBackground: unsafe { GetStockObject(brush) },
                    lpszClassName: name,
                    ..Default::default()
                };
                if unsafe { RegisterClassW(&class) } == 0 {
                    return Err(format!(
                        "Window picker registration failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            Ok(())
        })
        .clone()
        .map_err(picker_error)
}

/// Run on a blocking worker. `owner` is the live desktop host HWND, hidden by the
/// caller for the duration. The native loop and all handles end before returning.
pub fn pick_window_on_screen(owner: isize) -> Result<Option<WindowInfo>, RecorderError> {
    register_classes()?;
    let old_dpi =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if old_dpi.is_null() {
        return Err(picker_error(
            "Windows could not set physical coordinates for window selection.",
        ));
    }
    struct Windows {
        input: HWND,
        outline: HWND,
        dpi: *mut std::ffi::c_void,
        escape_registered: bool,
    }
    impl Drop for Windows {
        fn drop(&mut self) {
            unsafe {
                if !self.outline.is_null() {
                    DestroyWindow(self.outline);
                }
                if !self.input.is_null() {
                    if self.escape_registered {
                        UnregisterHotKey(self.input, 1);
                    }
                    SetWindowLongPtrW(self.input, GWLP_USERDATA, 0);
                    DestroyWindow(self.input);
                }
                SetThreadDpiAwarenessContext(self.dpi);
            }
        }
    }
    // Declared before the guard so callback state outlives native destruction.
    let mut picker = Box::new(Picker {
        outline: null_mut(),
        hover: None,
        pressed: None,
        selected: None,
        finished: false,
        failure: None,
    });
    let mut windows = Windows {
        input: null_mut(),
        outline: null_mut(),
        dpi: old_dpi,
        escape_registered: false,
    };
    let instance = unsafe { GetModuleHandleW(null_mut()) };
    windows.input = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            INPUT_CLASS,
            windows_sys::w!("FREYM — Click a window · Esc to cancel"),
            WS_POPUP,
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
            owner as HWND,
            null_mut(),
            instance,
            null_mut(),
        )
    };
    if windows.input.is_null() {
        return Err(picker_error("Windows could not open window selection."));
    }
    windows.outline = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            FRAME_CLASS,
            windows_sys::w!("FREYM selection outline"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            windows.input,
            null_mut(),
            instance,
            null_mut(),
        )
    };
    if windows.outline.is_null() {
        return Err(picker_error("Windows could not open the window outline."));
    }
    picker.outline = windows.outline;
    if unsafe { SetLayeredWindowAttributes(windows.input, 0, 28, LWA_ALPHA) } == 0
        || unsafe { SetLayeredWindowAttributes(windows.outline, 0, 255, LWA_ALPHA) } == 0
    {
        return Err(picker_error("Windows could not display window selection."));
    }
    unsafe {
        SetWindowLongPtrW(
            windows.input,
            GWLP_USERDATA,
            (&mut *picker as *mut Picker) as isize,
        );
        ShowWindow(windows.input, SW_SHOWNOACTIVATE);
    }
    picker.update_hover()?;
    // Escape belongs only to this modal picker. A native hotkey catches even a
    // short press without activating the overlay or installing an input hook.
    windows.escape_registered =
        unsafe { RegisterHotKey(windows.input, 1, MOD_NOREPEAT, VK_ESCAPE as u32) } != 0;
    if !windows.escape_registered {
        return Err(picker_error(
            "Window selection could not reserve Escape. Choose a window from Source.",
        ));
    }
    // Keep the outline attached when a window moves/resizes beneath a stationary
    // pointer. The timer exists only for this modal selection loop.
    if unsafe { SetTimer(windows.input, 1, 50, None) } == 0 {
        return Err(picker_error("Windows could not track the window outline."));
    }
    let mut message = MSG::default();
    loop {
        if picker.finished {
            break;
        }
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result <= 0 {
            if result < 0 {
                return Err(picker_error("Window selection stopped unexpectedly."));
            }
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    let selected = picker.selected.take();
    let failure = picker.failure.take();
    drop(windows);
    if let Some(error) = failure {
        return Err(error);
    }
    selected.as_ref().map(validate_selection).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn escape_cancels_and_releases_the_native_overlay_and_hotkey() {
        let picker = std::thread::spawn(|| pick_window_on_screen(0));
        let deadline = Instant::now() + Duration::from_secs(5);
        let input = loop {
            let hwnd = unsafe { FindWindowW(INPUT_CLASS, null_mut()) };
            let mut pid = 0;
            if !hwnd.is_null() {
                unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
                if pid == std::process::id() {
                    break hwnd;
                }
            }
            if picker.is_finished() {
                panic!("picker initialization failed: {:?}", picker.join().unwrap());
            }
            if Instant::now() >= deadline {
                panic!("native picker did not open");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Post the documented hotkey notification to our own fixture overlay;
        // no input is injected into another app and no recording is started.
        assert_ne!(unsafe { PostMessageW(input, WM_HOTKEY, 1, 0) }, 0);
        assert_eq!(picker.join().unwrap().unwrap(), None);
        assert_eq!(unsafe { IsWindow(input) }, 0);
        assert_ne!(
            unsafe { RegisterHotKey(null_mut(), 1, MOD_NOREPEAT, VK_ESCAPE as u32) },
            0
        );
        assert_ne!(unsafe { UnregisterHotKey(null_mut(), 1) }, 0);
    }
}
