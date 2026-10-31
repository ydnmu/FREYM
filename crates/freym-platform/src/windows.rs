#![allow(unsafe_code)] // Audited Win32 shell calls; no media or GPUI dependency.
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint},
    UI::WindowsAndMessaging::{
        HWND_TOPMOST, IsZoomed, SPI_GETCLIENTAREAANIMATION, SW_HIDE, SW_MAXIMIZE, SW_RESTORE,
        SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, ShowWindowAsync,
        SystemParametersInfoW,
    },
};

pub fn reduced_motion() -> bool {
    let mut enabled = 1i32;
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        )
    };
    result != 0 && enabled == 0
}
/// Call only on the GPUI owning thread with a live HWND borrowed from its window.
pub fn set_window_visible(hwnd: isize, visible: bool) {
    unsafe {
        ShowWindow(hwnd as HWND, if visible { SW_SHOW } else { SW_HIDE });
    }
}
/// Call on the owning GPUI thread with a live HWND borrowed from its window.
/// GPUI's pinned Windows `zoom` only maximizes; restore the saved placement
/// when the window is already maximized. Async delivery avoids reentrant UI updates.
pub fn toggle_window_maximized(hwnd: isize) {
    unsafe {
        let hwnd = hwnd as HWND;
        let command = if IsZoomed(hwnd) != 0 {
            SW_RESTORE
        } else {
            SW_MAXIMIZE
        };
        ShowWindowAsync(hwnd, command);
    }
}
/// Full-display selection stays above other applications without changing its
/// DPI-derived bounds. This is called on its owning GPUI window thread.
pub fn set_selection_overlay(hwnd: isize) {
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}
/// GPUI Windows uses HMONITOR as its DisplayId at the pinned upstream revision.
pub fn monitor_handle(x: i32, y: i32, width: u32, height: u32) -> Option<u64> {
    let point = POINT {
        x: x.checked_add(i32::try_from(width / 2).ok()?)?,
        y: y.checked_add(i32::try_from(height / 2).ok()?)?,
    };
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) };
    (!monitor.is_null()).then_some(monitor as u64)
}
