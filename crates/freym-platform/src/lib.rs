//! Small application-shell boundary. Recording backends remain in frame-*.
pub mod hotkeys;
pub mod resources;
pub mod startup;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;
#[cfg(not(target_os = "windows"))]
pub fn reduced_motion() -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
pub fn set_window_visible(_: isize, _: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn set_selection_overlay(_: isize) {}
#[cfg(not(target_os = "windows"))]
pub fn monitor_handle(_: i32, _: i32, _: u32, _: u32) -> Option<u64> {
    None
}
