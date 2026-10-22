//! Real Windows top-level window discovery and window identity (FRAME-0204).
//!
//! Bindings come from the official `windows-sys` crate, like the display enumeration;
//! this module declares no SDK types of its own and keeps every unsafe operation inside
//! its reviewed native section.
//!
//! Documented APIs used:
//!
//! - `EnumWindows` (user32): the top-level windows of the calling thread's desktop.
//! - `IsWindowVisible`, `IsIconic`, `IsWindow`, `GetWindowTextW` and
//!   `GetWindowTextLengthW` (user32): the visible/minimized/titled facts and the
//!   liveness check.
//! - `GetWindowLongW` with `GWL_EXSTYLE` (user32): the tool-window exclusion.
//! - `GetWindowThreadProcessId` (user32): the owning process, which is part of the
//!   identity this module re-verifies.
//! - `DwmGetWindowAttribute` with `DWMWA_CLOAKED` and `DWMWA_EXTENDED_FRAME_BOUNDS`
//!   (dwmapi): the documented cloak flag and the frame rectangle that
//!   Windows.Graphics.Capture reports as a window item's size.
//!
//! # Identity
//!
//! A `WindowId` is `window:hwnd=0x<16 hex>;pid=<decimal>`. The handle is what the
//! capture path needs, and the owning process is what makes handle reuse detectable:
//! Windows recycles `HWND` values, so a handle that still exists but now belongs to
//! another process must never be captured as the window the caller selected. The window
//! *title* is deliberately not part of the identity; it is user-visible text that
//! changes constantly and is reported as information only.
//!
//! [`WindowIdentity::verify`] is the check the capture start and every capture poll run:
//! the handle must still exist and must still belong to the same process. A recycled
//! handle inside the *same* process cannot be told apart from the original window
//! through the fields this contract carries; that residual case is documented in the
//! window capture module and is why the capture path also relies on the capture item's
//! own lifetime.
//!
//! # Discovery rules
//!
//! A window is listed when it is visible, has a non-empty title, is not a tool window
//! and is not DWM-cloaked. Cloaked windows are the documented ones that exist but are
//! not composed (a suspended UWP app, a window on another virtual desktop); capturing
//! one produces no image, so it is not offered as a capture target. When the DWM cloak
//! query itself fails, `cloaked` is `None` — the state is unknown, and an unknown state
//! is not read as "not cloaked" by anything that can refuse a capture.
//!
//! The list is ordered deterministically: case-insensitive title, then process id, then
//! handle, so two enumerations of an unchanged desktop produce the same order.
//! The on-screen picker and exact identity validator additionally accept visible
//! child HWNDs and untitled/tool popups. Child capture is explicitly a crop of the
//! verified root's WGC surface, with GetWindowRect in physical pixels for its bounds.

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus, WindowId, WindowInfo};
use std::collections::HashMap;

/// Longest title this module reports. A longer title is truncated for display only;
/// truncation is never applied to identity.
const MAX_TITLE_CHARS: usize = 512;

/// Bounded number of windows one enumeration accepts. A desktop with more top-level
/// windows than this is outside the bounded scan, and the enumeration reports that
/// instead of returning a silently truncated list.
const MAX_WINDOWS: usize = 4096;

/// One window exactly as the platform reported it, before Frame mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedWindow {
    handle: isize,
    process_id: u32,
    title: String,
    visible: bool,
    minimized: bool,
    /// `None` when the DWM query for the cloak flag failed: the state is unknown, not
    /// "not cloaked".
    cloaked: Option<bool>,
    is_tool_window: bool,
    /// Physical source bounds: DWM for top-level windows, GetWindowRect for
    /// children. None when the query failed or reported an empty rectangle.
    frame_size: Option<(u32, u32)>,
}

/// The cheap per-poll observation of one window: what the identity check needs and
/// nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Liveness {
    process_id: u32,
    visible: bool,
    minimized: bool,
}

/// A window identity re-resolved against the live desktop.
///
/// It is the only thing the capture path trusts about a selected window: the handle it
/// must capture and the process that handle belonged to when the caller selected it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowIdentity {
    handle: isize,
    process_id: u32,
}

/// The live state of a selected window, observed once per verification.
///
/// Deliberately narrow: this is what the per-poll identity check reads, and it uses only
/// the cheap window-manager queries (existence, owning process, visibility, iconified
/// state). The DWM facts a *start* decides on are read once, by
/// [`WindowIdentity::start_observation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowState {
    pub(crate) visible: bool,
    pub(crate) minimized: bool,
}

impl WindowState {
    /// One bounded line of the observed window state, for diagnostics.
    pub(crate) fn describe(&self) -> String {
        format!(
            "window_state: visible={} minimized={}",
            self.visible, self.minimized
        )
    }
}

/// The start-time observation of a selected window.
///
/// It adds the two documented DWM facts a start decides on — whether the window is
/// cloaked (it exists but is not composed, so nothing can be captured) and the frame
/// bounds that Windows.Graphics.Capture reports as the item size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowStart {
    pub(crate) state: WindowState,
    /// `None` when the DWM cloak query failed on this observation.
    pub(crate) cloaked: Option<bool>,
    /// Physical selected-source size, None when its bounds query failed.
    pub(crate) frame_size: Option<(u32, u32)>,
}

impl WindowStart {
    /// One bounded line of the start observation, for diagnostics.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} cloaked={}",
            self.state.describe(),
            match self.cloaked {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown (the DWM cloak query failed)",
            }
        )
    }
}

impl WindowIdentity {
    /// Parses a `WindowId` back into the handle and process it names.
    ///
    /// A malformed identity is an invalid request, never a window that happens to
    /// match: this contract has no way to express "approximately this window".
    pub(crate) fn parse(id: &str) -> Result<Self, RecorderError> {
        let rest = id.strip_prefix(IDENTITY_PREFIX).ok_or_else(|| {
            invalid_identity(format!(
                "The window identity {id:?} does not start with {IDENTITY_PREFIX:?}, so it does not name a Windows window."
            ))
        })?;
        let (handle_part, process_part) = rest.split_once(IDENTITY_SEPARATOR).ok_or_else(|| {
            invalid_identity(format!(
                "The window identity {id:?} is missing its {IDENTITY_SEPARATOR:?} process field."
            ))
        })?;
        let handle_text = handle_part.strip_prefix(HANDLE_PREFIX).ok_or_else(|| {
            invalid_identity(format!(
                "The window identity {id:?} is missing its {HANDLE_PREFIX:?} handle field."
            ))
        })?;
        let process_text = process_part.strip_prefix(PROCESS_PREFIX).ok_or_else(|| {
            invalid_identity(format!(
                "The window identity {id:?} is missing its {PROCESS_PREFIX:?} field."
            ))
        })?;
        if handle_text.len() != HANDLE_DIGITS {
            return Err(invalid_identity(format!(
                "The window identity {id:?} does not carry a {HANDLE_DIGITS}-digit handle."
            )));
        }
        let handle = u64::from_str_radix(handle_text, 16).map_err(|_| {
            invalid_identity(format!(
                "The window identity {id:?} does not carry a hexadecimal handle."
            ))
        })?;
        let process_id = process_text.parse::<u32>().map_err(|_| {
            invalid_identity(format!(
                "The window identity {id:?} does not carry a decimal process id."
            ))
        })?;
        if handle == 0 || process_id == 0 {
            return Err(invalid_identity(format!(
                "The window identity {id:?} names a null handle or process, which is not a window."
            )));
        }
        Ok(Self {
            handle: isize::try_from(handle).map_err(|_| {
                invalid_identity(format!(
                    "The window identity {id:?} carries a handle that does not fit this platform."
                ))
            })?,
            process_id,
        })
    }

    /// The raw window handle, for the capture interop factory. Crate-private: no native
    /// handle crosses the public API.
    pub(crate) fn handle(&self) -> isize {
        self.handle
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.process_id
    }

    /// Child HWNDs are subregions of their root's WGC surface, never desktop crops.
    /// Keep both identities so closure/reparenting cannot silently retarget capture.
    pub(crate) fn capture_root(&self) -> Result<Self, RecorderError> {
        self.verify()?;
        native::capture_root(self.handle).ok_or_else(|| {
            source_lost_error("The selected window has no live capture root.".into())
        })
    }

    pub(crate) fn frame_bounds(&self) -> Option<windows_sys::Win32::Foundation::RECT> {
        native::query_bounds(self.handle as _)
    }

    /// The canonical identity string for this handle and process.
    pub(crate) fn id(&self) -> WindowId {
        WindowId(format!(
            "{IDENTITY_PREFIX}{HANDLE_PREFIX}{:0width$X}{IDENTITY_SEPARATOR}{PROCESS_PREFIX}{}",
            u64::try_from(self.handle).unwrap_or(0),
            self.process_id,
            width = HANDLE_DIGITS
        ))
    }

    /// Re-resolves this identity against the live desktop.
    ///
    /// This is the per-poll check — it runs before every captured frame — so it reads only
    /// the cheap window-manager facts: whether the handle still exists, whether it still
    /// belongs to the selected process, and whether the window is visible or iconified.
    /// Every failure is an explicit [`ErrorCode::SourceLost`] naming which fact changed,
    /// because both cases mean the selected source is gone: the window was destroyed, or
    /// the handle was recycled by another process.
    pub(crate) fn verify(&self) -> Result<WindowState, RecorderError> {
        let observed = native::observe_liveness(self.handle).ok_or_else(|| {
            source_lost_error(format!(
                "The selected window {} no longer exists, so it cannot be captured.",
                self.id().0
            ))
        })?;
        if observed.process_id != self.process_id {
            return Err(source_lost_error(format!(
                "The window handle {} now belongs to process {} instead of the selected process {}, so the identity was reused and the session was not started.",
                self.id().0,
                observed.process_id,
                self.process_id
            )));
        }
        Ok(WindowState {
            visible: observed.visible,
            minimized: observed.minimized,
        })
    }

    /// The full start-time observation: the poll state plus the two DWM facts a start
    /// decides on.
    ///
    /// It is read once per session, because the DWM queries it needs are not part of the
    /// per-poll path.
    pub(crate) fn start_observation(&self) -> Result<WindowStart, RecorderError> {
        let observed = native::observe_window(self.handle).ok_or_else(|| {
            source_lost_error(format!(
                "The selected window {} no longer exists, so it cannot be captured.",
                self.id().0
            ))
        })?;
        if observed.process_id != self.process_id {
            return Err(source_lost_error(format!(
                "The window handle {} now belongs to process {} instead of the selected process {}, so the identity was reused and the session was not started.",
                self.id().0,
                observed.process_id,
                self.process_id
            )));
        }
        Ok(WindowStart {
            state: WindowState {
                visible: observed.visible,
                minimized: observed.minimized,
            },
            cloaked: observed.cloaked,
            frame_size: observed.frame_size,
        })
    }

    /// Whether the window is acceptable as a capture source right now.
    ///
    /// A cloaked window is refused: Windows.Graphics.Capture composes nothing for it, so
    /// starting a session would promise a live image that cannot arrive. A minimized or
    /// hidden window is *not* refused — it is reported instead, because its frames resume
    /// when it is shown again.
    pub(crate) fn require_capturable(&self, cloaked: Option<bool>) -> Result<(), RecorderError> {
        if cloaked == Some(true) {
            return Err(RecorderError {
                code: ErrorCode::SourceLost,
                message: format!(
                    "The selected window {} is DWM-cloaked (it exists but is not composed, for example on another virtual desktop or suspended), so no live image can be captured from it.",
                    self.id().0
                ),
                recovery: RecoveryStatus::NoRecording,
                action:
                    "Bring the window onto the current desktop and start a new capture session."
                        .to_string(),
            });
        }
        Ok(())
    }
}

/// Enumerates the top-level windows that can be offered as capture targets.
pub(crate) fn enumerate_windows() -> Result<Vec<WindowInfo>, RecorderError> {
    // Several windows can belong to one application. Resolve optional display
    // metadata once per process instead of opening that process for every card.
    let mut process_names: HashMap<u32, Option<String>> = HashMap::new();
    Ok(discover()?
        .into_iter()
        .map(|observed| {
            let process_name = process_names
                .entry(observed.process_id)
                .or_insert_with(|| native::process_executable_name(observed.process_id))
                .clone();
            WindowInfo {
                id: WindowIdentity {
                    handle: observed.handle,
                    process_id: observed.process_id,
                }
                .id(),
                title: observed.title,
                process_id: Some(observed.process_id),
                process_name,
            }
        })
        .collect())
}

/// Resolve an exact HWND, including a child, untitled dialog or tool popup that
/// is deliberately absent from the compact top-level chooser list.
pub(crate) fn validate_selection(id: &WindowId) -> Result<WindowInfo, RecorderError> {
    let identity = WindowIdentity::parse(&id.0)?;
    if identity.id() != *id {
        return Err(invalid_identity(
            "The window identity is not canonical.".into(),
        ));
    }
    let observed = native::observe_window(identity.handle())
        .ok_or_else(|| source_lost_error("The selected window no longer exists.".into()))?;
    if !selectable(&observed) || native::is_shell(identity.handle() as _) {
        return Err(source_lost_error(
            "The selected window is no longer available.".into(),
        ));
    }
    let current = identity.start_observation()?;
    identity.require_capturable(current.cloaked)?;
    let root = identity.capture_root()?;
    let title = if root != identity {
        let parent = native::observe_window(root.handle()).ok_or_else(|| {
            source_lost_error("The selected child window's parent closed.".into())
        })?;
        let label = if observed.title.trim().is_empty() {
            parent.title
        } else {
            observed.title
        };
        format!(
            "{} — child window (parent window capture)",
            if label.trim().is_empty() {
                "Untitled"
            } else {
                &label
            }
        )
    } else if observed.title.trim().is_empty() {
        "Untitled dialog or popup".into()
    } else {
        observed.title
    };
    identity.verify()?;
    Ok(WindowInfo {
        id: identity.id(),
        title,
        process_id: Some(identity.process_id()),
        process_name: native::process_executable_name(identity.process_id()),
    })
}

/// Read the frontmost window under a physical desktop point, excluding our picker.
/// Uses exact selection rules, independent of the compact top-level chooser list.
pub(crate) fn window_at_point(
    x: i32,
    y: i32,
) -> Option<(WindowId, windows_sys::Win32::Foundation::RECT)> {
    native::window_at_point(x, y)
}

/// Human-readable window discovery evidence for the read-only list mode.
///
/// Diagnostic text for a person to read, not a data API: it reports the same single
/// observation the list is produced from, including the facts the canonical `WindowInfo`
/// cannot carry (visible, minimized, cloak state, frame size).
pub fn window_diagnostics() -> Result<String, RecorderError> {
    let observed = native::observe_windows()?;
    let mut text = String::new();
    text.push_str("Frame Windows window discovery (FRAME-0204)\n");
    text.push_str("Read-only discovery: it enumerates top-level windows and captures nothing.\n");
    text.push_str(&format!(
        "observed: {} top-level window(s)\n",
        observed.len()
    ));
    text.push_str(
        "rules: visible, titled, not a tool window, not DWM-cloaked (the title is information, never identity)\n",
    );
    for window in observed.iter().filter(|window| admissible(window)) {
        let identity = WindowIdentity {
            handle: window.handle,
            process_id: window.process_id,
        };
        text.push_str(&format!(
            "  {} pid={} visible={} minimized={} cloaked={:?} frame={:?} title={:?}\n",
            identity.id().0,
            window.process_id,
            window.visible,
            window.minimized,
            window.cloaked,
            window.frame_size,
            window.title
        ));
    }
    Ok(text)
}

/// The bounded discovery pass, in its deterministic order.
pub(crate) fn discover() -> Result<Vec<ObservedWindow>, RecorderError> {
    let observed = native::observe_windows()?;
    let mut discovered: Vec<ObservedWindow> = observed
        .into_iter()
        .filter(admissible)
        .take(MAX_WINDOWS)
        .collect();
    discovered.sort_by(|left, right| {
        left.title
            .to_lowercase()
            .cmp(&right.title.to_lowercase())
            .then(left.process_id.cmp(&right.process_id))
            .then(left.handle.cmp(&right.handle))
    });
    Ok(discovered)
}

/// Whether one observed window is offered as a capture target.
fn admissible(window: &ObservedWindow) -> bool {
    selectable(window) && !window.is_tool_window && !window.title.trim().is_empty()
}

fn selectable(window: &ObservedWindow) -> bool {
    window.visible
        && window.cloaked != Some(true)
        && window.process_id != 0
        && window.handle != 0
        && window.frame_size.is_some()
}

/// Prefix and field spelling of a window identity. One place, so discovery and parsing
/// cannot drift apart.
const IDENTITY_PREFIX: &str = "window:";
const HANDLE_PREFIX: &str = "hwnd=0x";
const PROCESS_PREFIX: &str = "pid=";
const IDENTITY_SEPARATOR: &str = ";";
/// Always sixteen hexadecimal digits, so an identity is byte-comparable.
const HANDLE_DIGITS: usize = 16;

fn invalid_identity(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidRequest,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Copy a window identity from the enumerated window list and try again.".to_string(),
    }
}

fn source_lost_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action:
            "Refresh the window list and start a new capture session; Frame does not retarget a running session."
                .to_string(),
    }
}

/// Truncates a title for display only. Identity never contains the title.
fn bounded_title(title: &str) -> String {
    if title.chars().count() <= MAX_TITLE_CHARS {
        return title.to_string();
    }
    let mut bounded: String = title.chars().take(MAX_TITLE_CHARS).collect();
    bounded.push('…');
    bounded
}

#[allow(unsafe_code)]
mod native {
    use super::{Liveness, MAX_WINDOWS, ObservedWindow, WindowIdentity, bounded_title, selectable};
    use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
    use std::ffi::c_void;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx,
        EnumWindows, GA_ROOT, GW_HWNDNEXT, GWL_EXSTYLE, GetAncestor, GetDesktopWindow,
        GetShellWindow, GetTopWindow, GetWindow, GetWindowLongW, GetWindowRect,
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
        IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    pub(super) fn is_shell(hwnd: HWND) -> bool {
        hwnd == unsafe { GetDesktopWindow() } || hwnd == unsafe { GetShellWindow() }
    }

    pub(super) fn capture_root(handle: isize) -> Option<WindowIdentity> {
        let root = unsafe { GetAncestor(handle as HWND, GA_ROOT) };
        let live = observe_liveness(root as isize)?;
        (live.process_id != 0 && !is_shell(root)).then_some(WindowIdentity {
            handle: root as isize,
            process_id: live.process_id,
        })
    }

    /// DWM omits invisible resize borders; child HWNDs use their physical window rect.
    pub(super) fn query_bounds(hwnd: HWND) -> Option<RECT> {
        let mut bounds = RECT::default();
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        let dwm = if root == hwnd {
            (unsafe {
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut bounds as *mut RECT).cast(),
                    std::mem::size_of::<RECT>() as u32,
                )
            }) == 0
        } else {
            false
        };
        if !dwm {
            let old =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
            if old.is_null() {
                return None;
            }
            let ok = unsafe { GetWindowRect(hwnd, &mut bounds) } != 0;
            unsafe { SetThreadDpiAwarenessContext(old) };
            if !ok {
                return None;
            }
        }
        (bounds.right > bounds.left && bounds.bottom > bounds.top).then_some(bounds)
    }

    pub(super) fn window_at_point(x: i32, y: i32) -> Option<(frame_contracts::WindowId, RECT)> {
        // Bound the Z-order walk even if another process changes windows during it.
        let mut hwnd = unsafe { GetTopWindow(std::ptr::null_mut()) };
        for _ in 0..MAX_WINDOWS {
            if hwnd.is_null() {
                break;
            }
            let current = hwnd;
            hwnd = unsafe { GetWindow(current, GW_HWNDNEXT) };
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(current, &mut pid) };
            if pid == std::process::id()
                || unsafe { IsWindowVisible(current) } == 0
                || unsafe { IsIconic(current) } != 0
                || query_cloaked(current) == Some(true)
            {
                continue;
            }
            let Some(bounds) = query_bounds(current) else {
                continue;
            };
            if x < bounds.left || x >= bounds.right || y < bounds.top || y >= bounds.bottom {
                continue;
            }
            if is_shell(current) {
                return None;
            }
            // Descend the hit child hierarchy, bounded even while another process
            // changes it. This does not send messages into the target application.
            let mut target = current;
            for _ in 0..64 {
                let mut point = POINT { x, y };
                let old = unsafe {
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
                };
                if old.is_null() {
                    break;
                }
                let mapped = unsafe { ScreenToClient(target, &mut point) } != 0;
                let child = if mapped {
                    unsafe {
                        ChildWindowFromPointEx(
                            target,
                            point,
                            CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
                        )
                    }
                } else {
                    std::ptr::null_mut()
                };
                unsafe { SetThreadDpiAwarenessContext(old) };
                if child.is_null() || child == target {
                    break;
                }
                target = child;
            }
            let observed = observe(target);
            let bounds = query_bounds(target)?;
            return selectable(&observed).then(|| {
                (
                    WindowIdentity {
                        handle: target as isize,
                        process_id: observed.process_id,
                    }
                    .id(),
                    bounds,
                )
            });
        }
        None
    }

    /// Optional chooser label. Only the executable basename leaves this module;
    /// failures never hide an otherwise selectable window.
    pub(super) fn process_executable_name(pid: u32) -> Option<String> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        result.ok()?;
        let length = usize::try_from(length).ok()?;
        if length == 0 || length > path.len() {
            return None;
        }
        let full_path = String::from_utf16(&path[..length]).ok()?;
        let name = full_path.rsplit(['\\', '/']).next()?.trim();
        (!name.is_empty()).then(|| name.chars().take(128).collect())
    }

    /// Every top-level window of this desktop, as one `EnumWindows` walk reported it.
    ///
    /// The walk is bounded: more than [`MAX_WINDOWS`] windows is an explicit error
    /// instead of a silently truncated list.
    pub(super) fn observe_windows() -> Result<Vec<ObservedWindow>, RecorderError> {
        let mut collected: Vec<ObservedWindow> = Vec::new();
        let mut overflow = false;
        let mut context = WalkContext {
            collected: &mut collected,
            overflow: &mut overflow,
        };
        // SAFETY: `EnumWindows` calls the callback with each top-level window handle of
        // the calling thread's desktop; the pointer is a live stack reference for the
        // whole call, and the callback only appends plain data to it.
        let result = unsafe {
            EnumWindows(
                Some(enumerate_callback),
                (&mut context as *mut WalkContext<'_>) as isize,
            )
        };
        if result == 0 {
            return Err(RecorderError {
                code: ErrorCode::Internal,
                message: "EnumWindows failed, so this desktop's window list cannot be read."
                    .to_string(),
                recovery: RecoveryStatus::NoRecording,
                action: "Refresh the window list; if it persists, report the Windows build."
                    .to_string(),
            });
        }
        if overflow {
            return Err(RecorderError {
                code: ErrorCode::Internal,
                message: format!(
                    "The desktop reported more than {MAX_WINDOWS} top-level windows, which is outside the bounded window scan."
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Close unused windows and refresh the source list.".to_string(),
            });
        }
        Ok(collected)
    }

    /// Observes one window handle, or `None` when it no longer exists.
    ///
    /// This is the full observation the discovery pass and a session *start* use; it
    /// includes the DWM queries. The per-poll identity check uses
    /// [`observe_liveness`] instead, which touches only the window manager.
    pub(super) fn observe_window(handle: isize) -> Option<ObservedWindow> {
        let raw = handle as HWND;
        // SAFETY: `IsWindow` accepts any handle value and only answers whether it is a
        // window; no pointer is dereferenced and no state is modified.
        if unsafe { IsWindow(raw) } == 0 {
            return None;
        }
        Some(observe(raw))
    }

    /// The cheap liveness observation used before every captured frame: existence, owning
    /// process, visibility and iconified state, and nothing else.
    ///
    /// The window title, the extended style and both DWM facts are deliberately not read
    /// here: this runs on every capture poll, and the per-poll decision needs none of
    /// them.
    pub(super) fn observe_liveness(handle: isize) -> Option<Liveness> {
        let raw = handle as HWND;
        // SAFETY: `IsWindow` accepts any handle value and only answers whether it is a
        // window; no pointer is dereferenced and no state is modified.
        if unsafe { IsWindow(raw) } == 0 {
            return None;
        }
        let mut process_id = 0u32;
        // SAFETY: the handle is a live window handle and the out-parameter is a valid
        // initialized integer.
        unsafe { GetWindowThreadProcessId(raw, &mut process_id) };
        Some(Liveness {
            process_id,
            // SAFETY: documented as valid on any window handle; a hidden window is simply
            // reported as not visible.
            visible: unsafe { IsWindowVisible(raw) } != 0,
            // SAFETY: same.
            minimized: unsafe { IsIconic(raw) } != 0,
        })
    }

    struct WalkContext<'a> {
        collected: &'a mut Vec<ObservedWindow>,
        overflow: &'a mut bool,
    }

    impl WalkContext<'_> {
        fn observe(&mut self, hwnd: HWND) {
            if self.collected.len() >= MAX_WINDOWS {
                *self.overflow = true;
                return;
            }
            self.collected.push(observe(hwnd));
        }
    }

    /// Reads every fact one window observation carries.
    fn observe(hwnd: HWND) -> ObservedWindow {
        let mut process_id = 0u32;
        // SAFETY: the handle is a live window handle and the out-parameter is a valid
        // initialized integer.
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
        // SAFETY: a documented read-only style query on a live window handle.
        let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };
        // SAFETY: a documentedly clamped length query, valid on any window handle.
        let title_length = unsafe { GetWindowTextLengthW(hwnd) };
        let mut buffer = vec![
            0u16;
            usize::try_from(title_length.max(0))
                .unwrap_or(0)
                .min(super::MAX_TITLE_CHARS)
                + 2
        ];
        // SAFETY: the buffer holds the reported length plus a terminator, and the call
        // writes at most the buffer length it is given.
        let written = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        buffer.truncate(usize::try_from(written.max(0)).unwrap_or(0));
        ObservedWindow {
            handle: hwnd as isize,
            process_id,
            title: bounded_title(&String::from_utf16_lossy(&buffer)),
            // SAFETY: documented as valid on any window handle; a hidden window is
            // simply reported as not visible.
            visible: unsafe { IsWindowVisible(hwnd) } != 0,
            // SAFETY: same.
            minimized: unsafe { IsIconic(hwnd) } != 0,
            cloaked: query_cloaked(unsafe { GetAncestor(hwnd, GA_ROOT) }),
            is_tool_window: ex_style as u32 & WS_EX_TOOLWINDOW != 0,
            frame_size: query_frame_size(hwnd),
        }
    }

    /// The DWM cloak flag: `Some(true)` for cloaked, `Some(false)` for composed,
    /// `None` when the query itself failed and the state is therefore unknown.
    fn query_cloaked(hwnd: HWND) -> Option<bool> {
        let mut cloaked = 0u32;
        // SAFETY: the out-parameter is a valid initialized `u32`, and the size matches
        // the attribute the DWM documents for `DWMWA_CLOAKED`.
        let result = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast::<c_void>(),
                u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4),
            )
        };
        (result == 0).then_some(cloaked != 0)
    }

    /// The window's extended frame bounds size, which is what a
    /// Windows.Graphics.Capture window item reports as its size.
    fn query_frame_size(hwnd: HWND) -> Option<(u32, u32)> {
        let rect = query_bounds(hwnd)?;
        let width = rect.right.saturating_sub(rect.left);
        let height = rect.bottom.saturating_sub(rect.top);
        if width <= 0 || height <= 0 {
            return None;
        }
        Some((
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        ))
    }

    /// # Safety
    ///
    /// `lparam` must be the `*mut WalkContext` passed to `EnumWindows`, valid for the
    /// duration of that call.
    unsafe extern "system" fn enumerate_callback(
        hwnd: HWND,
        lparam: LPARAM,
    ) -> windows_sys::core::BOOL {
        // SAFETY: the caller guarantees a live `WalkContext` at this address for the
        // whole walk, and the callback only runs inside `EnumWindows`.
        let context = unsafe { &mut *(lparam as *mut WalkContext<'_>) };
        context.observe(hwnd);
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed() -> ObservedWindow {
        ObservedWindow {
            handle: 0x100,
            process_id: 7,
            title: "Document".to_string(),
            visible: true,
            minimized: false,
            cloaked: Some(false),
            is_tool_window: false,
            frame_size: Some((640, 480)),
        }
    }

    #[test]
    fn identities_round_trip_and_name_handle_and_process() {
        let identity = WindowIdentity {
            handle: 0x0000_0000_0001_2345,
            process_id: 4242,
        };
        let id = identity.id();
        assert_eq!(id.0, "window:hwnd=0x0000000000012345;pid=4242");
        assert_eq!(WindowIdentity::parse(&id.0).unwrap(), identity);
    }

    #[test]
    fn chooser_reads_only_the_current_process_executable_name() {
        let expected = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            native::process_executable_name(std::process::id()).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn malformed_identities_are_invalid_requests() {
        for malformed in [
            "",
            "not-a-window",
            "window:",
            "window:hwnd=0x1234;pid=1",
            "window:hwnd=0x0000000000001234",
            "window:hwnd=0x000000000000123Z;pid=1",
            "window:hwnd=0x0000000000000000;pid=1",
            "window:hwnd=0x0000000000001234;pid=0",
            "window:hwnd=0x0000000000001234;pid=not-a-number",
            "display:hwnd=0x0000000000001234;pid=1",
        ] {
            let error =
                WindowIdentity::parse(malformed).expect_err("a malformed identity is rejected");
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{malformed:?}");
            assert!(!error.message.is_empty());
        }
    }

    #[test]
    fn tool_windows_and_cloaked_windows_are_not_offered() {
        assert!(admissible(&observed()));
        assert!(
            !admissible(&ObservedWindow {
                visible: false,
                ..observed()
            }),
            "a hidden window is not offered"
        );
        assert!(
            !admissible(&ObservedWindow {
                is_tool_window: true,
                ..observed()
            }),
            "a tool window is not offered"
        );
        assert!(
            !admissible(&ObservedWindow {
                cloaked: Some(true),
                ..observed()
            }),
            "a cloaked window is not offered"
        );
        assert!(
            !admissible(&ObservedWindow {
                title: "   ".to_string(),
                ..observed()
            }),
            "a titleless window is not offered"
        );
        assert!(
            admissible(&ObservedWindow {
                cloaked: None,
                ..observed()
            }),
            "an unknown cloak state stays admissible: a failed query is not read as not-cloaked"
        );
        assert!(
            admissible(&ObservedWindow {
                minimized: true,
                ..observed()
            }),
            "a minimized window remains selectable: its behaviour is reported, not hidden"
        );
    }

    #[test]
    fn titles_are_bounded_for_display() {
        let long = "t".repeat(MAX_TITLE_CHARS + 10);
        let bounded = bounded_title(&long);
        assert_eq!(bounded.chars().count(), MAX_TITLE_CHARS + 1);
        assert!(bounded.ends_with('…'));
        assert_eq!(bounded_title("short"), "short");
    }
}
