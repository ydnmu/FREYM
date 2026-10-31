//! Windows display enumeration (FRAME-0101).
//!
//! Bindings come from the official `windows-sys` crate; the module declares no SDK
//! types of its own and keeps every unsafe operation inside this reviewed native
//! module.
//!
//! Documented APIs used:
//!
//! - `EnumDisplayMonitors` and `GetMonitorInfoW` (user32): the monitors of the
//!   calling thread's desktop, their rectangles and the primary flag.
//! - `EnumDisplayDevicesW` with `EDD_GET_DEVICE_INTERFACE_NAME` (user32): the
//!   monitor device-interface path that becomes the stable `DisplayId`.
//! - `EnumDisplaySettingsExW` with `ENUM_CURRENT_SETTINGS` (user32): the current
//!   display mode and its orientation.
//! - `SetThreadDpiAwarenessContext`, `GetThreadDpiAwarenessContext`,
//!   `GetAwarenessFromDpiAwarenessContext` and `AreDpiAwarenessContextsEqual`
//!   (user32): the per-monitor DPI context for the call, and its restoration.
//! - `CreateWindowExW`, `DestroyWindow`, `MonitorFromWindow`,
//!   `GetWindowDpiAwarenessContext` and `GetDpiForWindow` (user32): the documented
//!   DPI-aware per-monitor DPI query, through a temporary helper window.
//!
//! # DPI strategy
//!
//! Monitor rectangles are virtualized for a caller that is not per-monitor aware
//! (measured: 1707x1067 and 96 DPI for a 2560x1600 display at 150%), so the module
//! raises the *calling thread* to `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` for
//! the duration of the call and restores the previous context afterwards. The
//! process DPI awareness, the host window and every other thread are untouched.
//!
//! The effective per-monitor DPI is read with `GetDpiForWindow`, the query
//! Microsoft documents as the DPI-aware one: for a per-monitor aware window it
//! returns "the DPI of the monitor where the window is located", for a system-aware
//! window the system DPI, and for an unaware window the 96
//! `USER_DEFAULT_SCREEN_DPI` base value. That function needs a window, so
//! enumeration creates a temporary, invisible, top-level helper window inside the
//! target monitor on the thread that already holds the per-monitor context, and the
//! window inherits that context. `GetDpiForMonitor` is deliberately not used: its
//! own page states that it "is not DPI aware and should not be used if the calling
//! thread is per-monitor DPI aware", and this module no longer argues around that
//! remark.
//!
//! Helper window lifecycle, entirely inside one enumeration call on the calling
//! thread:
//!
//! - Created with the predefined `STATIC` system class, `WS_POPUP` and
//!   `WS_EX_NOACTIVATE`, 1x1, at the target monitor's origin. `WS_POPUP` without
//!   `WS_VISIBLE` is never shown; the system class supplies the window procedure,
//!   so no class is registered and no `WNDPROC` is written; `WS_EX_NOACTIVATE`
//!   prevents foreground activation by the system.
//! - The module never calls `ShowWindow`, `SetForegroundWindow` or
//!   `SetActiveWindow`, never parents the helper to a host window, and never uses,
//!   moves or modifies the host window.
//! - The window's own monitor association (`MonitorFromWindow` against the target
//!   `HMONITOR`) and its own DPI awareness context (`GetWindowDpiAwarenessContext`,
//!   documented as unaffected by the querying thread) are verified before the DPI
//!   is read. A null handle, a monitor mismatch, a non-per-monitor window and a zero
//!   DPI are all explicit errors.
//! - The window is destroyed explicitly with its result checked on the success path
//!   and on every error path, before the thread DPI context is restored. `Drop` only
//!   repeats the attempt as a last-resort safeguard. Creation, querying and
//!   destruction happen on the same thread, which `DestroyWindow` requires. There is
//!   no window cache, worker or background service.
//!
//! Message pump: none is added, and none is required for this usage. The helper
//! window is never shown or activated. Creation and destruction messages are
//! delivered synchronously and handled by the `STATIC` system class procedure;
//! this short-lived query does not depend on dispatching queued input. The module
//! never calls `GetMessage`, `PeekMessage` or `DispatchMessage`. Creating a window
//! can give the calling thread a message queue; destroying the window does not
//! undo that thread-level side effect.
//!
//! # What the rectangle comparison is not
//!
//! The rectangle from `GetMonitorInfoW` is compared with the current display mode
//! from `EnumDisplaySettingsExW` as a consistency guard against a mode change or a
//! disconnect race. Agreement is not presented as proof that the DPI query is
//! correct, and a mismatch is not diagnosed as a wrong DPI context: a pending
//! display-mode change produces the same disagreement. The current mode's
//! dmPelsWidth/dmPelsHeight already describe the visible surface, including its
//! orientation, so dimensions must match directly without another rotation.
//! Portrait geometry has synthetic regression coverage, not hardware evidence.
//!
//! # Baseline
//!
//! `SetThreadDpiAwarenessContext`, `GetDpiForWindow` and
//! `GetWindowDpiAwarenessContext` are documented as Windows 10 1607, but
//! `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` was introduced in Windows 10 1703.
//! The module's API baseline is therefore Windows 10 1703, measured on Windows 11
//! build 26200. These functions are statically imported, so on a Windows build
//! without them the failure is an import/loader failure before any Frame code runs;
//! the structured capability error only covers builds where the APIs exist but
//! reject the requested context.
//!
//! # Refresh rate (FRAME-0102)
//!
//! `refresh_rate_hz` carries the exact rate of the display's *current mode*, read
//! once per observation from the active display configuration
//! (`QueryDisplayConfig` with `QDC_ONLY_ACTIVE_PATHS`) and associated with each
//! monitor through two documented matches: the configuration target's
//! `monitorDevicePath` must equal the stable identity, and the configuration
//! source's `viewGdiDeviceName` must equal the adapter output the monitor was
//! enumerated under. Zero or several matches, an output mismatch and a target that
//! is no longer available are explicit errors, so no other monitor's rate can be
//! reported. A path that Windows specifies no rate for (a zero numerator *and*
//! denominator) yields `None`, not a substituted number.
//!
//! The rate is a nominal mode property, not a measurement: it is neither the rate
//! of unique captured frames nor the encoded frame rate, and a variable-refresh
//! display may present frames outside it. Those observations stay `null` until
//! their own stages measure them. The conversion and association rules live in
//! [`crate::display_rate`].
//!
//! `hdr_available` stays `false` because no HDR capability query exists yet. Window
//! enumeration, audio enumeration and encoder probing remain explicitly
//! unsupported, and display discovery enables no recording capability.

use frame_contracts::{DisplayId, DisplayInfo, ErrorCode, RecorderError, RecoveryStatus};

use crate::display_rate::{ActiveRateRecord, find_active_rate};

/// Upper bound on monitor devices accepted for one adapter output. Windows reports
/// one monitor per `\\.\DISPLAYn` output; a longer list means the association
/// cannot be trusted, so identity is rejected instead of guessed.
const MAX_MONITOR_DEVICES_PER_ADAPTER: usize = 16;

/// One monitor device as reported by `EnumDisplayDevicesW`.
///
/// A transient native observation, converted immediately into canonical displays.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorDeviceRecord {
    /// Index in the documented zero-based monitor device sequence of the adapter.
    index: u32,
    /// `DISPLAY_DEVICE_ATTACHED_TO_DESKTOP`, the documented flag for selecting the
    /// display devices that are on the desktop.
    attached_to_desktop: bool,
    /// `DISPLAY_DEVICE_MIRRORING_DRIVER`: an invisible pseudo-monitor, not a
    /// capture target and not a display identity.
    mirroring_driver: bool,
    /// Monitor device-interface path; empty when Windows supplies none.
    interface_path: String,
    /// OS monitor description. Informational text only, never identity.
    description: String,
}

/// Stable identity resolved for one desktop monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedMonitor {
    device_path: String,
    description: String,
}

/// The adapter's current display mode, as reported by `EnumDisplaySettingsExW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayMode {
    width: u32,
    height: u32,
    /// The mode reports a 90 or 270 degree orientation.
    quarter_turned: bool,
}

/// One monitor exactly as the platform reports it, before Frame mapping.
#[derive(Debug, Clone, PartialEq)]
/// One native display observation.
///
/// `monitor_handle` and the rectangle are transient evidence for the FRAME-0103
/// capture bridge's association checks within one start; they are never identity and
/// are not persisted.
struct NativeDisplay {
    /// Stable monitor device-interface path, for example
    /// `\\?\DISPLAY#BOE0D5E#5&12ac677&0&UID8448#{...}`.
    device_path: String,
    /// OS monitor description. Informational text only, never identity.
    description: String,
    /// Adapter output the monitor was enumerated under, for example `\\.\DISPLAY1`.
    /// Association input and evidence; never identity.
    adapter_name: String,
    /// Physical pixel width.
    width: u32,
    /// Physical pixel height.
    height: u32,
    /// Physical virtual-desktop x position.
    x: i32,
    /// Physical virtual-desktop y position.
    y: i32,
    /// Effective DPI; 96 is 100% scaling.
    dpi: u32,
    is_primary: bool,
    /// Full desktop rectangle (left, top, right, bottom) from the same monitor
    /// observation, used to compare against DXGI's output rectangle.
    desktop_rect: (i32, i32, i32, i32),
    /// Transient `HMONITOR` address for the association check only.
    monitor_handle: isize,
}

/// One native observation: the desktop monitors and the active
/// display-configuration paths, both read during the same enumeration pass.
///
/// The two parts are deliberately read together and mapped together, so a display's
/// rate and the evidence reported beside it come from one observation instead of two
/// independent queries.
#[derive(Debug, Clone, PartialEq)]
struct NativeObservation {
    monitors: Vec<NativeDisplay>,
    rates: Vec<ActiveRateRecord>,
}

/// One enumerated display together with the display-configuration record its rate was
/// mapped from.
///
/// This pairing is private: it exists so the backend and the diagnostics report are
/// produced from the same observation, and it is never part of the public contract.
#[derive(Debug)]
struct ObservedDisplay {
    info: DisplayInfo,
    rate: ActiveRateRecord,
    /// Full desktop rectangle seen with this display, for the FRAME-0103 capture
    /// bridge's geometry comparison.
    desktop_rect: (i32, i32, i32, i32),
    /// Transient `HMONITOR` address seen with this display, for the capture bridge's
    /// monitor-association comparison only.
    monitor_handle: isize,
}

/// One complete enumeration observation: the canonical display values and the
/// private rate evidence they were derived from.
#[derive(Debug)]
struct DisplayObservation {
    displays: Vec<ObservedDisplay>,
}

impl DisplayObservation {
    /// The canonical display values, in display order.
    fn into_displays(self) -> Vec<DisplayInfo> {
        self.displays
            .into_iter()
            .map(|display| display.info)
            .collect()
    }
}

/// Read-only native observation source.
///
/// The Windows implementation queries Win32. Tests substitute a visibly synthetic
/// source to exercise empty and failure results; no synthetic source is reachable
/// from the production path.
trait DisplayObservationSource {
    fn observe(&self) -> Result<NativeObservation, RecorderError>;
}

/// Enumerates the attached displays in deterministic, human-readable order.
#[cfg(target_os = "windows")]
pub(crate) fn enumerate_displays() -> Result<Vec<DisplayInfo>, RecorderError> {
    Ok(observe()?.into_displays())
}

/// Human-readable display diagnostics for the read-only probe.
///
/// This is diagnostic text for a person to read, not a data API: it exposes neither
/// the native rational nor the association records, and its only contract is
/// `Result<String, RecorderError>`. It is produced from the same single observation
/// as [`enumerate_displays`], so the values it prints and the evidence beside them
/// belong to the same pass.
#[cfg(target_os = "windows")]
pub fn display_diagnostics() -> Result<String, RecorderError> {
    Ok(render_diagnostics(&observe()?))
}

/// The current output behind one stable display identity, as capture selection needs
/// it (FRAME-0103).
///
/// Crate-private: it re-uses this module's enumeration and identity rules instead of
/// persisting a monitor handle, an ordinal or a GDI name as identity, and it adds no
/// public surface.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CaptureOutput {
    /// Stable monitor device-interface path, for example
    /// `\\?\DISPLAY#BOE0D5E#5&12ac677&0&UID8448#{...}`.
    pub(crate) device_path: String,
    /// Adapter output the monitor is enumerated under, for example `\\.\DISPLAY1`.
    /// It is matched against `DXGI_OUTPUT_DESC.DeviceName`; it is not identity.
    pub(crate) adapter_name: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) is_primary: bool,
    /// The accepted FRAME-0102 nominal mode rate for this display, if known.
    pub(crate) refresh_rate_hz: Option<f64>,
    /// Full desktop rectangle (left, top, right, bottom) observed with this display.
    pub(crate) desktop_rect: (i32, i32, i32, i32),
    /// Transient `HMONITOR` address observed with this display, for the capture
    /// bridge's association check only. Never identity.
    pub(crate) monitor_handle: isize,
}

impl CaptureOutput {
    /// Whether a re-resolution still describes the same output, geometry and monitor
    /// association.
    ///
    /// This compares every observed field the capture path relies on — stable identity,
    /// adapter output name, mode dimensions, the full desktop rectangle and the
    /// transient monitor association — so a display that moved or changed monitor
    /// between the two reads fails the post-initialization recheck. It is a comparison
    /// of two separate observations, not an atomic snapshot: a change that lands
    /// between the reads can still go undetected.
    pub(crate) fn same_output(&self, other: &Self) -> bool {
        self.device_path.eq_ignore_ascii_case(&other.device_path)
            && self.adapter_name.eq_ignore_ascii_case(&other.adapter_name)
            && self.width == other.width
            && self.height == other.height
            && self.desktop_rect == other.desktop_rect
            && self.monitor_handle == other.monitor_handle
    }
}

/// Resolves the current output behind a stable display identity.
///
/// A stale or unknown identity is an explicit error, never a substituted display.
#[cfg(target_os = "windows")]
pub(crate) fn capture_output(display_id: &str) -> Result<CaptureOutput, RecorderError> {
    let observation = observe()?;
    let display = observation
        .displays
        .iter()
        .find(|display| display.info.id.0.eq_ignore_ascii_case(display_id))
        .ok_or_else(|| {
            source_error(
                format!(
                    "The selected display {display_id} is not among the currently attached displays, so its output cannot be resolved."
                ),
                "Enumerate displays again and select a display that is currently attached.",
            )
        })?;
    Ok(CaptureOutput {
        device_path: display.info.id.0.clone(),
        adapter_name: display.rate.source_gdi_device_name.clone(),
        width: display.info.width,
        height: display.info.height,
        is_primary: display.info.is_primary,
        refresh_rate_hz: display.info.refresh_rate_hz,
        desktop_rect: display.desktop_rect,
        monitor_handle: display.monitor_handle,
    })
}

/// Runs one enumeration observation and maps it.
///
/// Both public entry points use this path, so neither performs a second
/// active-display query for its own purposes.
#[cfg(target_os = "windows")]
fn observe() -> Result<DisplayObservation, RecorderError> {
    observe_from(&native::Win32DisplaySource::default())
}

/// Maps one source observation. Kept separate from the native source so the mapping
/// rules can be driven by synthetic observations in tests.
fn observe_from(
    source: &impl DisplayObservationSource,
) -> Result<DisplayObservation, RecorderError> {
    map_observation(source.observe()?)
}

fn map_observation(observation: NativeObservation) -> Result<DisplayObservation, RecorderError> {
    map_displays(observation.monitors, &observation.rates)
}

/// Resolves the monitor device that an adapter output presents.
///
/// `EnumDisplayDevicesW` documents the association as the monitor device at index
/// zero of that adapter name, and `DISPLAY_DEVICE_ATTACHED_TO_DESKTOP` as the flag
/// that selects devices on the desktop. The association is verified rather than
/// assumed: exactly one desktop-attached monitor device must exist, at index zero,
/// and it must not be a mirroring pseudo-monitor. Every other outcome is rejected
/// explicitly instead of inventing an identity.
fn resolve_monitor_device(
    adapter: &str,
    devices: &[MonitorDeviceRecord],
) -> Result<ResolvedMonitor, RecorderError> {
    if devices.len() > MAX_MONITOR_DEVICES_PER_ADAPTER {
        return Err(source_error(
            format!(
                "Windows reported {} monitor devices for adapter output {adapter}; the monitor association cannot be trusted.",
                devices.len()
            ),
            "Restart the display driver and enumerate again.",
        ));
    }

    let attached: Vec<&MonitorDeviceRecord> = devices
        .iter()
        .filter(|device| device.attached_to_desktop)
        .collect();
    let device = match attached.as_slice() {
        [] => {
            return Err(source_error(
                format!(
                    "Windows reported no desktop-attached monitor device for adapter output {adapter}."
                ),
                "Reconnect the display and enumerate again.",
            ));
        }
        [only] => *only,
        several => {
            return Err(source_error(
                format!(
                    "Windows reported {} desktop-attached monitor devices for adapter output {adapter}; the monitor identity is ambiguous.",
                    several.len()
                ),
                "Disconnect the extra display and enumerate again.",
            ));
        }
    };

    if device.index != 0 {
        return Err(source_error(
            format!(
                "Windows reported the only desktop-attached monitor device for adapter output {adapter} at index {}; the documented association is index zero, so identity is rejected rather than guessed.",
                device.index
            ),
            "Reconnect the display and enumerate again.",
        ));
    }
    if device.mirroring_driver {
        return Err(source_error(
            format!(
                "Windows reported a mirroring-driver pseudo-monitor for adapter output {adapter}, which has no display identity."
            ),
            "Remove the mirroring display driver and enumerate again.",
        ));
    }
    if device.interface_path.is_empty() {
        return Err(source_error(
            format!(
                "Windows reported no usable monitor device interface path for adapter output {adapter}; the session adapter name is not durable identity."
            ),
            "Reconnect the display and enumerate again.",
        ));
    }

    Ok(ResolvedMonitor {
        device_path: device.interface_path.clone(),
        description: device.description.clone(),
    })
}

/// Compares the monitor rectangle with the adapter's current display mode.
///
/// This is a consistency guard for a display-mode change or a disconnect race, and
/// it is not treated as evidence about the DPI query: a mismatch does not say which
/// of the two reads is stale, so both causes are reported instead of diagnosing the
/// DPI context. Current mode dimensions describe the visible device surface and
/// already account for orientation; applying another rotation would be incorrect.
fn check_rectangle_against_mode(
    adapter: &str,
    rectangle: (u32, u32),
    mode: DisplayMode,
) -> Result<(), RecorderError> {
    let matches = rectangle == (mode.width, mode.height);
    if matches {
        return Ok(());
    }
    Err(source_error(
        format!(
            "Windows reported a {}x{} rectangle for adapter output {adapter} while its current display mode is {}x{} (orientation {}); geometry is inconsistent, possibly because the display mode or desktop layout changed during enumeration.",
            rectangle.0,
            rectangle.1,
            mode.width,
            mode.height,
            if mode.quarter_turned {
                "90 or 270 degrees"
            } else {
                "0 or 180 degrees"
            }
        ),
        "Retry enumeration; if it persists, report the Windows build and display adapter.",
    ))
}

/// Converts one native observation into canonical displays plus their private rate
/// evidence, in display order.
///
/// Any observation that cannot be reported truthfully - missing or duplicated stable
/// identity, empty geometry, an unusable DPI or an association failure - fails the
/// whole enumeration instead of producing a display with invented metadata. The rate
/// is associated here, from the same observation, so the value and the evidence
/// reported beside it cannot come from different passes.
fn map_displays(
    native: Vec<NativeDisplay>,
    rates: &[ActiveRateRecord],
) -> Result<DisplayObservation, RecorderError> {
    let mut displays = Vec::with_capacity(native.len());
    for monitor in native {
        if monitor.device_path.is_empty() {
            return Err(source_error(
                "Windows reported a display without a stable device interface path.".to_string(),
                "Reconnect the display and enumerate again; if it persists, report the display adapter.",
            ));
        }
        if displays
            .iter()
            .any(|existing: &ObservedDisplay| existing.info.id.0 == monitor.device_path)
        {
            return Err(source_error(
                format!(
                    "Windows reported the display identity {} more than once, so the monitor association is ambiguous.",
                    monitor.device_path
                ),
                "Reconnect the display and enumerate again.",
            ));
        }
        if monitor.width == 0 || monitor.height == 0 {
            return Err(source_error(
                format!(
                    "Windows reported an empty rectangle for display {}.",
                    monitor.device_path
                ),
                "Reconnect the display and enumerate again.",
            ));
        }
        if monitor.dpi == 0 {
            return Err(source_error(
                format!(
                    "Windows reported an unusable DPI for display {}.",
                    monitor.device_path
                ),
                "Retry enumeration; if it persists, restart Frame and report the error.",
            ));
        }

        let rate = find_active_rate(rates, &monitor.device_path, &monitor.adapter_name)?;
        displays.push(ObservedDisplay {
            info: DisplayInfo {
                id: DisplayId(monitor.device_path),
                name: monitor.description,
                width: monitor.width,
                height: monitor.height,
                x: monitor.x,
                y: monitor.y,
                scale_factor: f64::from(monitor.dpi) / 96.0,
                // The nominal rate of the current mode. Capture and encode rates are
                // separate observations and stay null until their stages measure them.
                refresh_rate_hz: rate.refresh.map(|native| native.hz()),
                is_primary: monitor.is_primary,
                // No HDR capability query is implemented, so this stays conservative.
                hdr_available: false,
                required_recording_quality: None,
            },
            rate: rate.clone(),
            desktop_rect: monitor.desktop_rect,
            monitor_handle: monitor.monitor_handle,
        });
    }
    order_displays(&mut displays);
    label_unnamed_displays(&mut displays);
    Ok(DisplayObservation { displays })
}

/// Primary first, then left to right and top to bottom, with the stable identity
/// as the final tie-break so the order never depends on enumeration order. The rate
/// evidence stays attached to its display while the pairs are reordered.
fn order_displays(displays: &mut [ObservedDisplay]) {
    displays.sort_by(|a, b| {
        b.info
            .is_primary
            .cmp(&a.info.is_primary)
            .then(a.info.x.cmp(&b.info.x))
            .then(a.info.y.cmp(&b.info.y))
            .then_with(|| a.info.id.cmp(&b.info.id))
    });
}

/// Some adapters report no monitor description. The ordinal label is presentation
/// only: it is derived from the deterministic order, is not an identity, and is
/// never persisted.
fn label_unnamed_displays(displays: &mut [ObservedDisplay]) {
    for (index, display) in displays.iter_mut().enumerate() {
        if display.info.name.is_empty() {
            display.info.name = format!("Display {}", index + 1);
        }
    }
}

/// Renders the human-readable diagnostics report for one observation.
///
/// The report is text for a person: it states what the rate is and is not, prints the
/// canonical display values and, per display, the display-configuration evidence the
/// rate came from. All of it comes from the single observation it is handed.
fn render_diagnostics(observation: &DisplayObservation) -> String {
    let mut report = String::new();
    report.push_str("Frame display diagnostics (FRAME-0101, FRAME-0102)\n\n");
    if observation.displays.is_empty() {
        report.push_str("Windows reports no attached displays in this session.\n");
        return report;
    }

    report.push_str(&format!(
        "Windows display enumeration: {} display(s)\n",
        observation.displays.len()
    ));
    for line in [
        "A refresh value is the nominal rate of that display's current mode.",
        "It is not a capture rate, not an encoded rate and not a measurement of delivered frames.",
        "Everything below comes from one enumeration pass.",
        "The individual Windows API reads in that pass are not atomic, so a display mode or",
        "topology change during the pass is reported as an explicit error where it is detected;",
        "otherwise a value can still belong to the previous mode until the next enumeration.",
    ] {
        report.push_str(line);
        report.push('\n');
    }
    report.push('\n');

    for (index, display) in observation.displays.iter().enumerate() {
        let info = &display.info;
        let rate = &display.rate;
        let primary = if info.is_primary { " (primary)" } else { "" };
        report.push_str(&format!("{}. {}{}\n", index + 1, info.name, primary));
        report.push_str(&format!(
            "   size: {}x{} px (physical)  position: {},{}\n",
            info.width, info.height, info.x, info.y
        ));
        report.push_str(&format!(
            "   scale: {}  effective dpi: {}\n",
            info.scale_factor,
            info.scale_factor * 96.0
        ));
        match info.refresh_rate_hz {
            Some(hertz) => report.push_str(&format!(
                "   refresh: {hertz} Hz\n"
            )),
            None => report.push_str(
                "   refresh: unknown (Windows returned an all-zero rate for this path; no value is substituted)\n",
            ),
        }
        report.push_str("   hdr available: no (no HDR capability query is implemented)\n");
        report.push_str(&format!("   id: {}\n", info.id.0));
        report.push_str(&format!(
            "   rate evidence: source={} target={} name={:?} rational={} available={} output_technology={} status_flags=0x{:X}\n",
            rate.source_gdi_device_name,
            rate.target_monitor_device_path,
            rate.target_friendly_name,
            rate.refresh
                .map(|native| native.to_string())
                .unwrap_or_else(|| "none".to_string()),
            rate.target_available,
            rate.output_technology,
            rate.status_flags
        ));
    }

    report
}

/// Keeps the original failure and appends a cleanup failure, preserving the
/// original error code.
fn merge_cleanup_failure(
    mut primary: RecorderError,
    cleanup: RecorderError,
    cleanup_description: &str,
) -> RecorderError {
    primary.message = format!(
        "{} In addition, {cleanup_description}: {}",
        primary.message, cleanup.message
    );
    primary
}

/// Keeps the original error when the thread DPI context cannot be restored.
fn merge_restore_failure(primary: RecorderError, restore: RecorderError) -> RecorderError {
    merge_cleanup_failure(
        primary,
        restore,
        "the thread DPI awareness context could not be restored",
    )
}

/// Keeps the original error when the helper window cannot be destroyed.
fn merge_window_cleanup_failure(primary: RecorderError, cleanup: RecorderError) -> RecorderError {
    merge_cleanup_failure(
        primary,
        cleanup,
        "the helper DPI query window could not be destroyed",
    )
}

/// Display enumeration has failed; it runs outside recording, so no recovery
/// status is claimed.
fn source_error(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

/// The requested DPI context does not exist on this Windows build, so display
/// geometry and scale cannot be read truthfully. This is a capability gap, not a
/// lost source. It covers only builds where the APIs exist and reject the context;
/// see the module documentation about the static import baseline.
fn unsupported_context_error() -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message: "Windows rejected the per-monitor-v2 DPI awareness context, so display pixel geometry and scale cannot be read without virtualization.".to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Run display enumeration on Windows 10 version 1703 or newer.".to_string(),
    }
}

/// The caller's DPI context could not be restored; enumeration must not leave the
/// calling thread changed.
fn restore_error(detail: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows display enumeration could not restore the calling thread DPI awareness context: {detail}."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Restart Frame before enumerating displays again.".to_string(),
    }
}

/// The temporary helper window could not be created or destroyed.
fn window_error(detail: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows display enumeration could not manage its temporary DPI query window: {detail}"
        ),
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

/// Runs `body` with the calling thread raised to per-monitor-v2 DPI awareness
/// (FRAME-0103 capture selection).
///
/// DXGI reports an output's desktop rectangle in the coordinate space of the calling
/// process's DPI awareness, so a capture worker that is not aware would compare a
/// virtualized rectangle against the physical geometry of display enumeration. This
/// reuses the enumeration's reviewed context guard instead of duplicating the DPI
/// handling: only the calling thread is affected, and the caller's context is
/// restored and verified before returning.
#[cfg(target_os = "windows")]
pub(crate) fn with_per_monitor_awareness<T>(
    body: impl FnOnce() -> Result<T, RecorderError>,
) -> Result<T, RecorderError> {
    let mut context = native::ThreadDpiContext::enter()?;
    let result = body();
    let restored = context.restore();
    match (result, restored) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(restore)) => Err(restore),
        (Err(error), Err(restore)) => Err(merge_restore_failure(error, restore)),
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native {
    use super::{
        DisplayMode, DisplayObservationSource, MAX_MONITOR_DEVICES_PER_ADAPTER,
        MonitorDeviceRecord, NativeDisplay, NativeObservation, RecorderError,
        check_rectangle_against_mode, merge_restore_failure, merge_window_cleanup_failure,
        resolve_monitor_device, restore_error, source_error, unsupported_context_error,
        window_error,
    };
    use std::cell::RefCell;
    use std::mem::size_of;
    use std::ptr;
    use windows_sys::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_SUPPORTED,
        ERROR_SUCCESS, HWND, LPARAM, RECT,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_MIRRORING_DRIVER,
        DISPLAY_DEVICEW, DMDO_90, DMDO_270, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW,
        EnumDisplayMonitors, EnumDisplaySettingsExW, GetMonitorInfoW, HDC, HMONITOR,
        MONITOR_DEFAULTTONULL, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_PER_MONITOR_AWARE,
        GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetThreadDpiAwarenessContext,
        GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, EDD_GET_DEVICE_INTERFACE_NAME, MONITORINFOF_PRIMARY,
        WS_EX_NOACTIVATE, WS_POPUP,
    };
    use windows_sys::core::BOOL;

    use crate::display_rate::{ActiveRateRecord, NativeRefreshRate};

    /// The system class used for the helper window. It is a predefined class, so no
    /// class registration and no application window procedure are needed.
    const HELPER_WINDOW_CLASS: &str = "STATIC";

    /// Upper bound on `QueryDisplayConfig` retries after the documented
    /// `ERROR_INSUFFICIENT_BUFFER` topology race. The documented remedy is to ask
    /// for the sizes again; retrying forever would hide a topology that never
    /// settles, so the bound turns that into an explicit error.
    const MAX_TOPOLOGY_RETRIES: usize = 4;

    /// Injection points used by the tests to drive the real error path.
    ///
    /// Never constructed in production builds, where the field that carries them is
    /// always `None`; the type exists so that production and test builds execute the
    /// same code path.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(not(test), allow(dead_code))]
    enum InjectedFault {
        /// Fail after the DPI context is raised and before the helper window exists.
        BeforeHelperWindow,
        /// Fail after the helper window exists and before the DPI is read.
        BeforeDpiRead,
    }

    /// One helper-window observation recorded for the tests.
    ///
    /// Populated only by test builds; the production recording functions are empty.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(not(test), allow(dead_code))]
    struct WindowAudit {
        hwnd: HWND,
        visible: bool,
        foreground: bool,
        destroyed: bool,
    }

    /// Test-only hooks: fault injection and helper-window auditing.
    ///
    /// Production builds never set a fault and never record an audit entry, so both
    /// members are inert there. They exist so the exercised code path is identical
    /// in tests and production; no global or thread-local state is involved.
    #[derive(Default)]
    struct ProbeHooks {
        fault: Option<InjectedFault>,
        #[cfg_attr(not(test), allow(dead_code))]
        audit: RefCell<Vec<WindowAudit>>,
    }

    impl ProbeHooks {
        fn injected_fault(&self) -> Option<InjectedFault> {
            self.fault
        }

        /// Records a helper window as created. Empty in production builds.
        fn record_created(&self, hwnd: HWND) {
            #[cfg(test)]
            {
                // SAFETY: read-only queries about a live window handle owned by the
                // calling thread.
                let visible = unsafe { IsWindowVisible(hwnd) } != 0;
                // SAFETY: returns the foreground window, which may be null.
                let foreground = unsafe { GetForegroundWindow() } == hwnd;
                self.audit.borrow_mut().push(WindowAudit {
                    hwnd,
                    visible,
                    foreground,
                    destroyed: false,
                });
            }
            #[cfg(not(test))]
            let _ = hwnd;
        }

        /// Records that a helper window was destroyed. Empty in production builds.
        fn record_destroyed(&self, hwnd: HWND) {
            #[cfg(test)]
            {
                if let Some(entry) = self
                    .audit
                    .borrow_mut()
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.hwnd == hwnd)
                {
                    entry.destroyed = true;
                }
            }
            #[cfg(not(test))]
            let _ = hwnd;
        }
    }

    #[cfg(test)]
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible};

    /// Native source backed by the documented user32 APIs.
    #[derive(Default)]
    pub(super) struct Win32DisplaySource {
        hooks: ProbeHooks,
    }

    impl DisplayObservationSource for Win32DisplaySource {
        fn observe(&self) -> Result<NativeObservation, RecorderError> {
            let mut context = ThreadDpiContext::enter()?;
            let observation = self.collect();
            // Helper windows are gone by now: `collect` destroys each one as soon as
            // its DPI has been read, so the thread context is restored last.
            // Restoration is checked on both the success and the failure path; the
            // caller must never be left in the elevated DPI context.
            match (observation, context.restore()) {
                (Ok(observation), Ok(())) => Ok(observation),
                (Err(error), Ok(())) => Err(error),
                (Ok(_), Err(restore)) => Err(restore),
                (Err(error), Err(restore)) => Err(merge_restore_failure(error, restore)),
            }
        }
    }

    impl Win32DisplaySource {
        /// Reads one observation: the desktop monitors and the active
        /// display-configuration paths, in one pass.
        ///
        /// The documented `NULL`/`NULL` combination of `EnumDisplayMonitors`
        /// enumerates every display monitor of the calling thread's desktop. The
        /// display-configuration query does not participate in DPI virtualization, so
        /// it does not depend on the raised thread context; it is read here so that the
        /// backend and the diagnostics report share a single observation instead of
        /// each performing its own query.
        fn collect(&self) -> Result<NativeObservation, RecorderError> {
            let rates = active_display_rates()?;
            let mut state = EnumerationState {
                displays: Vec::new(),
                failure: None,
                hooks: &self.hooks,
            };
            // SAFETY: a null device context and clipping rectangle enumerate the whole
            // desktop and pass a null HDC to the callback. `state` outlives the call
            // and is only reachable through the callback, which Windows invokes
            // synchronously on this thread.
            let enumerated = unsafe {
                EnumDisplayMonitors(
                    ptr::null_mut(),
                    ptr::null(),
                    Some(collect_monitor),
                    &mut state as *mut EnumerationState<'_> as LPARAM,
                )
            };
            if let Some(failure) = state.failure {
                return Err(failure);
            }
            if enumerated == 0 {
                return Err(source_error(
                    "Windows display enumeration failed: EnumDisplayMonitors reported failure."
                        .to_string(),
                    "Retry enumeration; if it persists, restart Frame and report the error.",
                ));
            }
            // A successful enumeration that reports zero monitors is a truthful empty
            // list; it is never a fabricated display.
            Ok(NativeObservation {
                monitors: state.displays,
                rates,
            })
        }
    }

    /// Per-call enumeration state, owned by the calling thread for the duration of
    /// the callback and never shared or cached.
    struct EnumerationState<'a> {
        displays: Vec<NativeDisplay>,
        failure: Option<RecorderError>,
        hooks: &'a ProbeHooks,
    }

    /// `EnumDisplayMonitors` callback.
    ///
    /// The callback only reaches through the state pointer that the enumerating call
    /// owns, stops enumeration on the first failure, and contains no
    /// `unwrap`/`expect`/indexing that can panic. A panic would abort through the
    /// non-unwinding ABI rather than unwind across the FFI boundary.
    unsafe extern "system" fn collect_monitor(
        monitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the live `*mut EnumerationState` passed to
        // EnumDisplayMonitors by `collect`; the callback runs synchronously on the
        // same thread, so the reference cannot outlive the value or alias another
        // reference.
        let state = unsafe { &mut *(data as *mut EnumerationState<'_>) };
        match observe_monitor(monitor, state) {
            Ok(display) => {
                state.displays.push(display);
                1
            }
            Err(failure) => {
                state.failure = Some(failure);
                0
            }
        }
    }

    fn observe_monitor(
        monitor: HMONITOR,
        state: &EnumerationState<'_>,
    ) -> Result<NativeDisplay, RecorderError> {
        // SAFETY: an all-zero `MONITORINFOEXW` is a valid initial state for this
        // plain data struct, and `cbSize` must be set to `sizeof(MONITORINFOEXW)`
        // for GetMonitorInfoW to fill `szDevice`.
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `monitor` is a live handle owned by EnumDisplayMonitors, and the
        // MONITORINFOEXW pointer is cast to its MONITORINFO prefix, which is what
        // the API expects for the extended structure.
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo as *mut MONITORINFO) } == 0 {
            return Err(source_error(
                "Windows display enumeration failed: GetMonitorInfoW reported failure.".to_string(),
                "Reconnect the display and enumerate again.",
            ));
        }

        let adapter = wide_to_string(&info.szDevice).ok_or_else(|| {
            source_error(
                "Windows returned a display adapter name that is not valid UTF-16.".to_string(),
                "Retry enumeration; if it persists, report the display adapter.",
            )
        })?;

        let device = resolve_monitor_device(&adapter, &monitor_devices(&adapter)?)?;

        let rect = info.monitorInfo.rcMonitor;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return Err(source_error(
                format!(
                    "Windows reported an empty rectangle for adapter output {adapter} ({width}x{height} pixels)."
                ),
                "Reconnect the display and enumerate again.",
            ));
        }

        if state.hooks.injected_fault() == Some(InjectedFault::BeforeHelperWindow) {
            return Err(source_error(
                "TEST ONLY fault injection: enumeration failed before the helper DPI query window was created."
                    .to_string(),
                "This error is produced only by tests.",
            ));
        }

        // The helper window lives only until its DPI has been read, so no window
        // outlives the DPI context it was created in.
        let window = DpiQueryWindow::create_in(monitor, rect, state.hooks)?;

        if state.hooks.injected_fault() == Some(InjectedFault::BeforeDpiRead) {
            return Err(window_failure(
                window,
                source_error(
                    "TEST ONLY fault injection: enumeration failed after the helper DPI query window was created."
                        .to_string(),
                    "This error is produced only by tests.",
                ),
                state.hooks,
            ));
        }

        let dpi = match window.dpi() {
            Ok(dpi) => dpi,
            Err(error) => return Err(window_failure(window, error, state.hooks)),
        };
        // Explicit, checked cleanup on the success path, before the thread DPI
        // context is restored. Only the failure path uses `window_failure`.
        window.destroy(state.hooks)?;

        let mode = current_mode(&adapter)?;
        check_rectangle_against_mode(&adapter, (width as u32, height as u32), mode)?;

        Ok(NativeDisplay {
            device_path: device.device_path,
            description: device.description,
            adapter_name: adapter,
            width: width as u32,
            height: height as u32,
            x: rect.left,
            y: rect.top,
            dpi,
            is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            desktop_rect: (rect.left, rect.top, rect.right, rect.bottom),
            monitor_handle: monitor as isize,
        })
    }

    /// Reads the active display-configuration paths and their native refresh rates.
    ///
    /// Private on purpose: the records this returns carry the raw rational and the
    /// association evidence, and they are consumed only through the single observation
    /// that also produces the canonical display values.
    ///
    /// `QueryDisplayConfig` documents that the sizes returned by
    /// `GetDisplayConfigBufferSizes` can become stale before the query, in which case
    /// it fails with `ERROR_INSUFFICIENT_BUFFER` and the caller is expected to ask for
    /// the sizes again. That loop is bounded by [`MAX_TOPOLOGY_RETRIES`].
    ///
    /// `QDC_ONLY_ACTIVE_PATHS` asks for the currently active paths. The optional
    /// `QDC_VIRTUAL_MODE_AWARE` flag is deliberately not used: this module reads the
    /// path's own rational rate and never interprets the mode-index union, so the flag
    /// would change nothing. `QDC_VIRTUAL_REFRESH_RATE_AWARE` is Windows 11 only and
    /// would break the Windows 10 1703 baseline.
    fn active_display_rates() -> Result<Vec<ActiveRateRecord>, RecorderError> {
        let flags = QDC_ONLY_ACTIVE_PATHS;
        for _ in 0..MAX_TOPOLOGY_RETRIES {
            let mut path_count = 0u32;
            let mut mode_count = 0u32;
            // SAFETY: both out-parameters point to initialized local storage and the
            // call only reports the sizes it would need.
            let sizes =
                unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
            if sizes != ERROR_SUCCESS {
                return Err(display_config_error("GetDisplayConfigBufferSizes", sizes));
            }

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            // SAFETY: the arrays are sized from the documented size query and
            // QueryDisplayConfig writes at most `path_count`/`mode_count` entries.
            // `currentTopologyId` must be null unless QDC_DATABASE_CURRENT is used, and
            // it is not used here.
            let result = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    ptr::null_mut(),
                )
            };
            if result == ERROR_INSUFFICIENT_BUFFER {
                // Documented topology race: the sizes are queried again.
                continue;
            }
            if result != ERROR_SUCCESS {
                // QueryDisplayConfig documents Win32 error codes as its return value,
                // so the code is reported directly instead of through GetLastError.
                return Err(display_config_error("QueryDisplayConfig", result));
            }

            paths.truncate(path_count as usize);
            return paths
                .iter()
                .map(active_rate_record)
                .collect::<Result<Vec<_>, _>>();
        }

        Err(source_error(
            format!(
                "Windows display enumeration failed: the active display configuration changed on every one of {MAX_TOPOLOGY_RETRIES} attempts, so the refresh rate could not be read reliably."
            ),
            "Retry enumeration; if it persists, close applications that change display topology.",
        ))
    }

    /// Reduces one active path to plain values, with the documented association
    /// evidence from `DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME` and
    /// `DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME`.
    fn active_rate_record(
        path: &DISPLAYCONFIG_PATH_INFO,
    ) -> Result<ActiveRateRecord, RecorderError> {
        let source = &path.sourceInfo;
        let target = &path.targetInfo;

        // SAFETY: an all-zero `DISPLAYCONFIG_SOURCE_DEVICE_NAME` is a valid starting
        // state; the header fields are set below as the API documents.
        let mut source_name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
        source_name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: source.adapterId,
            id: source.id,
        };
        // SAFETY: the header requests this documented structure and advertises its
        // size, and the call only writes into it.
        let result = unsafe { DisplayConfigGetDeviceInfo(&mut source_name.header) };
        if result != ERROR_SUCCESS as i32 {
            return Err(display_config_error(
                "DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME)",
                result as u32,
            ));
        }

        // SAFETY: as above, for the target name structure.
        let mut target_name: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { std::mem::zeroed() };
        target_name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: target.adapterId,
            id: target.id,
        };
        // SAFETY: the header requests this documented structure and advertises its
        // size, and the call only writes into it.
        let result = unsafe { DisplayConfigGetDeviceInfo(&mut target_name.header) };
        if result != ERROR_SUCCESS as i32 {
            return Err(display_config_error(
                "DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME)",
                result as u32,
            ));
        }

        // Both names are association evidence: unusable text means the rate cannot be
        // associated with a monitor, which is an explicit error rather than a guess.
        let source_gdi_device_name = wide_to_string(&source_name.viewGdiDeviceName).ok_or_else(|| {
            source_error(
                "Windows returned a display-configuration source name that is not valid UTF-16.".to_string(),
                "Retry enumeration; if it persists, report the Windows build and display adapter.",
            )
        })?;
        let target_monitor_device_path = wide_to_string(&target_name.monitorDevicePath).ok_or_else(|| {
            source_error(
                "Windows returned a display-configuration monitor device path that is not valid UTF-16.".to_string(),
                "Retry enumeration; if it persists, report the Windows build and display adapter.",
            )
        })?;

        Ok(ActiveRateRecord {
            source_gdi_device_name,
            target_monitor_device_path,
            // The friendly name is evidence only; a forced target may report none.
            target_friendly_name: wide_to_string(&target_name.monitorFriendlyDeviceName)
                .unwrap_or_default(),
            refresh: NativeRefreshRate::from_rational(
                target.refreshRate.Numerator,
                target.refreshRate.Denominator,
            )?,
            target_available: target.targetAvailable != 0,
            output_technology: target.outputTechnology as u32,
            status_flags: target.statusFlags,
        })
    }

    /// Builds an explicit error for a documented display-configuration failure code.
    fn display_config_error(api: &str, code: u32) -> RecorderError {
        let hint = match code {
            ERROR_ACCESS_DENIED => {
                " The process has no access to the console session, which is documented behavior for a remote session."
            }
            ERROR_NOT_SUPPORTED => {
                " The system is not running a WDDM graphics driver, which these display APIs require."
            }
            ERROR_GEN_FAILURE => {
                " The system reported an unspecified display-configuration failure."
            }
            _ => "",
        };
        source_error(
            format!("Windows display enumeration failed: {api} returned Win32 error {code}.{hint}"),
            "Retry enumeration; if it persists, restart Frame and report the error code.",
        )
    }

    /// Reports `error` after destroying the helper window, appending a cleanup
    /// failure when destruction itself fails.
    fn window_failure(
        window: DpiQueryWindow,
        error: RecorderError,
        hooks: &ProbeHooks,
    ) -> RecorderError {
        match window.destroy(hooks) {
            Ok(()) => error,
            Err(cleanup) => merge_window_cleanup_failure(error, cleanup),
        }
    }

    /// A temporary, invisible, top-level helper window whose only purpose is the
    /// documented per-monitor `GetDpiForWindow` query.
    ///
    /// See the module documentation for the full lifecycle contract. `destroy`
    /// reports failure explicitly; `Drop` only repeats the attempt as a safeguard.
    struct DpiQueryWindow {
        hwnd: HWND,
    }

    impl DpiQueryWindow {
        /// Creates the helper window inside `target` and verifies that it belongs to
        /// that monitor and inherited the per-monitor DPI context.
        fn create_in(
            target: HMONITOR,
            rect: RECT,
            hooks: &ProbeHooks,
        ) -> Result<Self, RecorderError> {
            let class = wide_buffer(HELPER_WINDOW_CLASS);
            let title = wide_buffer("Frame display DPI query");
            // `WS_POPUP` alone is never visible; `WS_EX_NOACTIVATE` prevents
            // foreground activation by the system. The window is a top-level window
            // (no parent, no owner) placed inside the target monitor's rectangle.
            // SAFETY: both string buffers are NUL-terminated and outlive the call.
            // The parent, menu and module handles are documented as optional, so a
            // predefined system class needs none of them, and no creation parameter
            // points outside this function.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_POPUP,
                    rect.left,
                    rect.top,
                    1,
                    1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                // CreateWindowExW documents GetLastError for extended error
                // information; no other call happens in between.
                let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                return Err(window_error(
                    format!("CreateWindowExW failed with error {code}."),
                    "Retry enumeration; if it persists, restart Frame and report the error.",
                ));
            }

            let window = Self { hwnd };
            hooks.record_created(hwnd);

            // The window must belong to the monitor whose DPI is being read.
            // MonitorFromWindow documents a null result when the rectangle does not
            // intersect any display.
            // SAFETY: read-only query about a live window handle owned by this thread.
            let owner = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
            if owner.is_null() || owner != target {
                return Err(window_failure(
                    window,
                    source_error(
                        format!(
                            "Windows placed the temporary DPI query window outside the monitor being enumerated (primary display mismatch for {target:?})."
                        ),
                        "Retry enumeration; if it persists, report the display layout.",
                    ),
                    hooks,
                ));
            }

            // The window inherits the creating thread's context, and
            // GetWindowDpiAwarenessContext reports the window's own context
            // regardless of the querying thread. A window that is not per-monitor
            // aware would silently yield the base or system DPI, so this is checked.
            // SAFETY: read-only queries about a live window handle owned by this thread.
            let awareness =
                unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd)) };
            if awareness != DPI_AWARENESS_PER_MONITOR_AWARE {
                return Err(window_failure(
                    window,
                    source_error(
                        format!(
                            "The temporary DPI query window reports DPI awareness {awareness} instead of per-monitor awareness, so its DPI would not be the monitor's effective DPI."
                        ),
                        "Retry enumeration; if it persists, restart Frame and report the error.",
                    ),
                    hooks,
                ));
            }

            Ok(window)
        }

        /// Reads the monitor's effective DPI through the documented DPI-aware query.
        fn dpi(&self) -> Result<u32, RecorderError> {
            // SAFETY: read-only query about a live window handle owned by this thread.
            let dpi = unsafe { GetDpiForWindow(self.hwnd) };
            if dpi == 0 {
                // GetDpiForWindow documents a zero result for an invalid window.
                return Err(source_error(
                    "GetDpiForWindow returned zero for the temporary DPI query window.".to_string(),
                    "Retry enumeration; if it persists, restart Frame and report the error.",
                ));
            }
            Ok(dpi)
        }

        /// Destroys the helper window, reporting failure explicitly.
        fn destroy(mut self, hooks: &ProbeHooks) -> Result<(), RecorderError> {
            let hwnd = self.hwnd;
            let result = self.destroy_inner();
            if result.is_ok() {
                hooks.record_destroyed(hwnd);
            }
            result
        }

        fn destroy_inner(&mut self) -> Result<(), RecorderError> {
            if self.hwnd.is_null() {
                return Ok(());
            }
            // SAFETY: the handle was created by `create_in` on this thread;
            // DestroyWindow documents that a thread cannot destroy a window created by
            // a different thread, which is why creation, query and destruction all
            // happen on the calling thread.
            let destroyed = unsafe { DestroyWindow(self.hwnd) };
            if destroyed == 0 {
                let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                return Err(window_error(
                    format!("DestroyWindow failed with error {code}."),
                    "Restart Frame; a hidden DPI query window may have been left behind.",
                ));
            }
            self.hwnd = ptr::null_mut();
            Ok(())
        }
    }

    impl Drop for DpiQueryWindow {
        fn drop(&mut self) {
            // Last-resort safeguard: an explicit `destroy` has already reported any
            // failure it saw, and Drop cannot report one.
            let _ = self.destroy_inner();
        }
    }

    /// Enumerates the monitor devices of one adapter output, as documented: index
    /// from zero upward until `EnumDisplayDevicesW` reports failure.
    fn monitor_devices(adapter: &str) -> Result<Vec<MonitorDeviceRecord>, RecorderError> {
        let adapter_name = wide_buffer(adapter);
        let mut devices = Vec::new();
        for index in 0..=MAX_MONITOR_DEVICES_PER_ADAPTER as u32 {
            // SAFETY: an all-zero `DISPLAY_DEVICEW` is a valid initial state and
            // `cb` must be set to `sizeof(DISPLAY_DEVICEW)` before the call.
            let mut device: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
            device.cb = size_of::<DISPLAY_DEVICEW>() as u32;
            // SAFETY: `adapter_name` is NUL-terminated and lives for the call, and
            // `device` is a correctly sized out-parameter. The interface-name flag
            // only changes how `DeviceID` is filled in.
            let enumerated = unsafe {
                EnumDisplayDevicesW(
                    adapter_name.as_ptr(),
                    index,
                    &mut device,
                    EDD_GET_DEVICE_INTERFACE_NAME,
                )
            };
            if enumerated == 0 {
                break;
            }
            devices.push(MonitorDeviceRecord {
                index,
                attached_to_desktop: device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0,
                mirroring_driver: device.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER != 0,
                // Data that is not valid UTF-16 becomes an empty path, which
                // `resolve_monitor_device` rejects explicitly. A description is a
                // presentation label only and may legitimately be absent.
                interface_path: wide_to_string(&device.DeviceID).unwrap_or_default(),
                description: wide_to_string(&device.DeviceString).unwrap_or_default(),
            });
        }
        Ok(devices)
    }

    /// Reads the adapter's current display mode and orientation.
    fn current_mode(adapter: &str) -> Result<DisplayMode, RecorderError> {
        let adapter_name = wide_buffer(adapter);
        // SAFETY: an all-zero `DEVMODEW` is a valid initial state and `dmSize` must
        // be set to `sizeof(DEVMODEW)` before the call.
        let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        // SAFETY: `adapter_name` is NUL-terminated and lives for the call, and
        // `mode` is a correctly sized out-parameter.
        let enumerated = unsafe {
            EnumDisplaySettingsExW(adapter_name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode, 0)
        };
        if enumerated == 0 {
            return Err(source_error(
                format!(
                    "Windows display enumeration failed: EnumDisplaySettingsExW reported failure for adapter output {adapter}."
                ),
                "Retry enumeration; if it persists, restart Frame and report the error.",
            ));
        }
        if mode.dmPelsWidth == 0 || mode.dmPelsHeight == 0 {
            return Err(source_error(
                format!(
                    "Windows reported an empty current display mode for adapter output {adapter}."
                ),
                "Reconnect the display and enumerate again.",
            ));
        }
        // Orientation is diagnostic only. dmPelsWidth/dmPelsHeight already describe
        // the current visible surface; do not rotate them again.
        let orientation = unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation };
        Ok(DisplayMode {
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            quarter_turned: orientation == DMDO_90 || orientation == DMDO_270,
        })
    }

    /// Temporarily raises the calling thread to per-monitor-v2 DPI awareness.
    ///
    /// Only the calling thread is affected: the process DPI awareness and any host
    /// window are untouched. `restore` reports failure explicitly and `Drop` only
    /// repeats the attempt as a cleanup safeguard.
    pub(super) struct ThreadDpiContext {
        previous: DPI_AWARENESS_CONTEXT,
        restored: bool,
    }

    impl ThreadDpiContext {
        pub(super) fn enter() -> Result<Self, RecorderError> {
            // SAFETY: the call sets the DPI awareness context of the calling thread
            // and returns the previous context, or a null handle when the requested
            // context is not valid on this Windows build.
            let previous =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
            if previous.is_null() {
                return Err(unsupported_context_error());
            }

            let mut context = Self {
                previous,
                restored: false,
            };
            // SAFETY: both calls take handles owned here and require only a live
            // calling thread.
            let awareness =
                unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
            if awareness != DPI_AWARENESS_PER_MONITOR_AWARE {
                let error = source_error(
                    format!(
                        "Windows reported DPI awareness {awareness} for the enumeration thread instead of per-monitor DPI awareness, so pixel geometry and scale would be virtualized."
                    ),
                    "Retry enumeration; if it persists, report the Windows build.",
                );
                return Err(match context.restore() {
                    Ok(()) => error,
                    Err(restore) => merge_restore_failure(error, restore),
                });
            }
            Ok(context)
        }

        /// Restores the caller's original thread DPI context and verifies it.
        pub(super) fn restore(&mut self) -> Result<(), RecorderError> {
            if self.restored {
                return Ok(());
            }
            // SAFETY: `previous` is the handle returned by the successful
            // SetThreadDpiAwarenessContext call that created this guard.
            let returned = unsafe { SetThreadDpiAwarenessContext(self.previous) };
            if returned.is_null() {
                return Err(restore_error(
                    "Windows rejected the previous thread DPI awareness context",
                ));
            }
            // SAFETY: both arguments are valid contexts; the documented comparison
            // ignores informational flags.
            let equal = unsafe {
                AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), self.previous)
            };
            if equal == 0 {
                return Err(restore_error(
                    "the thread DPI awareness context is not the one enumeration started with",
                ));
            }
            self.restored = true;
            Ok(())
        }
    }

    impl Drop for ThreadDpiContext {
        fn drop(&mut self) {
            // Cleanup safeguard only: a second failure cannot be reported from Drop,
            // and the explicit `restore` call has already reported the first one.
            let _ = self.restore();
        }
    }

    fn wide_buffer(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Converts a NUL-terminated wide buffer. `None` means the OS returned data
    /// that is not valid UTF-16, which callers treat as a hard failure for identity
    /// and as an absent label for a description.
    fn wide_to_string(buffer: &[u16]) -> Option<String> {
        let length = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        String::from_utf16(&buffer[..length]).ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::display::{check_rectangle_against_mode, enumerate_displays, observe_from};
        use frame_contracts::ErrorCode;
        use windows_sys::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
            DPI_AWARENESS_CONTEXT_UNAWARE,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, IsWindow, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
        };

        /// Thread DPI awareness contexts exercised by the verification tests.
        const AWARENESS_MODES: [(&str, DPI_AWARENESS_CONTEXT); 4] = [
            ("UNAWARE", DPI_AWARENESS_CONTEXT_UNAWARE),
            ("SYSTEM_AWARE", DPI_AWARENESS_CONTEXT_SYSTEM_AWARE),
            ("PER_MONITOR_AWARE", DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE),
            (
                "PER_MONITOR_AWARE_V2",
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            ),
        ];

        /// Enters a test DPI context for the calling thread.
        ///
        /// Returns `None` when this process cannot enter that context; the caller
        /// then reports an explicit skip, which is never hardware evidence.
        fn enter_test_context(
            label: &str,
            context: DPI_AWARENESS_CONTEXT,
        ) -> Option<DPI_AWARENESS_CONTEXT> {
            // SAFETY: the call only sets the calling thread's DPI awareness context
            // and returns the previous one, or null when it cannot be entered.
            let previous = unsafe { SetThreadDpiAwarenessContext(context) };
            if previous.is_null() {
                eprintln!(
                    "SKIP (not hardware evidence): this process cannot enter the {label} DPI awareness context."
                );
                return None;
            }
            Some(previous)
        }

        /// Restores a test context captured by `enter_test_context`.
        fn leave_test_context(label: &str, caller: DPI_AWARENESS_CONTEXT) {
            // SAFETY: `caller` came from a successful call on this thread.
            let returned = unsafe { SetThreadDpiAwarenessContext(caller) };
            assert!(
                !returned.is_null(),
                "the test DPI context must be restorable ({label})"
            );
        }

        fn has_attached_displays() -> bool {
            match enumerate_displays() {
                Ok(displays) => !displays.is_empty(),
                Err(error) => panic!("display enumeration must not fail: {error}"),
            }
        }

        /// Reports an explicit skip when this session has no display hardware.
        fn hardware_or_skip(what: &str) -> bool {
            if has_attached_displays() {
                return true;
            }
            eprintln!(
                "SKIP (not hardware evidence): {what} needs an attached display, and this Windows session reports none."
            );
            false
        }

        #[test]
        fn official_bindings_keep_the_documented_ffi_layout() {
            // Guards against a windows-sys upgrade changing a structure that is
            // filled in by the operating system.
            assert_eq!(size_of::<MONITORINFOEXW>(), 104);
            assert_eq!(size_of::<DISPLAY_DEVICEW>(), 840);
            assert_eq!(size_of::<DEVMODEW>(), 220);
            assert_eq!(std::mem::offset_of!(MONITORINFOEXW, szDevice), 40);
            assert_eq!(std::mem::offset_of!(DISPLAY_DEVICEW, DeviceID), 328);
            assert_eq!(std::mem::offset_of!(DEVMODEW, dmPelsWidth), 172);
        }

        #[test]
        fn wide_buffers_convert_and_missing_terminators_are_bounded() {
            let terminated: Vec<u16> = "\\\\.\\DISPLAY1".encode_utf16().chain([0]).collect();
            assert_eq!(
                wide_to_string(&terminated).as_deref(),
                Some("\\\\.\\DISPLAY1")
            );

            let unterminated: Vec<u16> = "AB".encode_utf16().collect();
            assert_eq!(wide_to_string(&unterminated).as_deref(), Some("AB"));

            assert_eq!(wide_to_string(&[0xD800u16]), None);

            let buffer = wide_buffer("\\\\.\\DISPLAY2");
            assert_eq!(buffer.last(), Some(&0));
            assert_eq!(wide_to_string(&buffer).as_deref(), Some("\\\\.\\DISPLAY2"));
        }

        #[test]
        fn missing_per_monitor_context_is_a_capability_error_with_the_baseline() {
            let error = unsupported_context_error();
            assert_eq!(error.code, ErrorCode::NotImplemented);
            assert_eq!(error.recovery, frame_contracts::RecoveryStatus::NoRecording);
            assert!(error.message.contains("per-monitor-v2"));
            assert!(error.action.contains("1703"), "action: {}", error.action);
        }

        #[test]
        fn restoration_and_window_failures_are_explicit() {
            let restore =
                restore_error("Windows rejected the previous thread DPI awareness context");
            assert_eq!(restore.code, ErrorCode::Internal);
            assert!(restore.message.contains("restore"));
            assert!(restore.message.contains("DPI awareness context"));

            let window = window_error("CreateWindowExW failed with error 5.".to_string(), "Retry.");
            assert_eq!(window.code, ErrorCode::Internal);
            assert!(
                window
                    .message
                    .contains("CreateWindowExW failed with error 5")
            );
            assert!(window.message.contains("DPI query window"));
        }

        #[test]
        fn thread_dpi_context_is_raised_and_restored_in_every_awareness_mode() {
            for (label, context) in AWARENESS_MODES {
                let Some(caller) = enter_test_context(label, context) else {
                    continue;
                };
                // The context the enumeration call must hand back unchanged: the
                // context that is active now, not the test's original context.
                // SAFETY: reads the calling thread's DPI awareness context.
                let expected = unsafe { GetThreadDpiAwarenessContext() };

                let mut raised = ThreadDpiContext::enter().unwrap_or_else(|error| {
                    panic!("cannot raise the thread context ({label}): {error}")
                });
                // SAFETY: both calls only read the calling thread's DPI context.
                let awareness =
                    unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
                assert_eq!(
                    awareness, DPI_AWARENESS_PER_MONITOR_AWARE,
                    "enumeration must run in a per-monitor DPI context ({label})"
                );

                raised
                    .restore()
                    .unwrap_or_else(|error| panic!("restoration must succeed ({label}): {error}"));
                // SAFETY: both arguments are valid contexts on this thread.
                let restored = unsafe {
                    AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), expected)
                };
                assert_eq!(
                    restored, 1,
                    "the caller's DPI context must be restored ({label})"
                );
                // A second restore is a no-op rather than a second native call.
                raised
                    .restore()
                    .expect("restoring an already restored context must succeed");

                leave_test_context(label, caller);
            }
        }

        #[test]
        fn enumeration_preserves_the_calling_thread_context_and_reports_identical_displays() {
            let baseline = enumerate_displays().expect("enumeration must not fail");

            for (label, context) in AWARENESS_MODES {
                let Some(caller) = enter_test_context(label, context) else {
                    continue;
                };
                // SAFETY: reads the calling thread's DPI awareness context.
                let before = unsafe { GetThreadDpiAwarenessContext() };
                let observed = enumerate_displays()
                    .unwrap_or_else(|error| panic!("enumeration must work ({label}): {error}"));
                // SAFETY: both arguments are valid contexts on this thread.
                let after = unsafe { GetThreadDpiAwarenessContext() };
                let preserved = unsafe { AreDpiAwarenessContextsEqual(after, before) };
                leave_test_context(label, caller);

                assert_eq!(
                    preserved, 1,
                    "enumeration must leave the caller in its own DPI context ({label})"
                );
                assert_eq!(
                    observed, baseline,
                    "physical geometry, scale and order must not depend on the caller's DPI awareness ({label})"
                );
            }
        }

        #[test]
        fn reported_scale_matches_the_system_dpi_virtualization() {
            let displays = enumerate_displays().expect("enumeration must not fail");
            if displays.len() != 1 {
                eprintln!(
                    "SKIP (not hardware evidence): the virtualization cross-check needs exactly one attached display, found {}.",
                    displays.len()
                );
                return;
            }

            let Some(caller) = enter_test_context("UNAWARE", DPI_AWARENESS_CONTEXT_UNAWARE) else {
                return;
            };
            // SAFETY: reads documented system metrics; an unaware context receives
            // the virtualized virtual-screen size.
            let (virtual_width, virtual_height) = unsafe {
                (
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            leave_test_context("UNAWARE", caller);

            assert!(
                virtual_width > 0 && virtual_height > 0,
                "an unaware context must still report a virtual screen"
            );

            let display = &displays[0];
            let width_scale = f64::from(display.width) / f64::from(virtual_width as u32);
            let height_scale = f64::from(display.height) / f64::from(virtual_height as u32);
            for (axis, measured) in [("width", width_scale), ("height", height_scale)] {
                assert!(
                    (measured - display.scale_factor).abs() < 0.01,
                    "reported scale {} must match the {axis} ratio {measured} between physical {} and virtual pixels",
                    display.scale_factor,
                    display.width
                );
            }
        }

        #[test]
        fn successful_enumeration_creates_and_destroys_an_invisible_helper_window() {
            if !hardware_or_skip("the helper-window lifecycle check") {
                return;
            }

            let source = Win32DisplaySource::default();
            let displays = observe_from(&source)
                .expect("enumeration must not fail")
                .into_displays();
            let audit = source.hooks.audit.borrow();

            assert!(
                !audit.is_empty(),
                "the documented DPI query must create one helper window per attached display"
            );
            assert_eq!(
                audit.len(),
                displays.len(),
                "one helper window per enumerated display, none cached"
            );
            for entry in audit.iter() {
                assert!(
                    !entry.visible,
                    "the helper window must never be visible: {entry:?}"
                );
                assert!(
                    !entry.foreground,
                    "the helper window must never take the foreground: {entry:?}"
                );
                assert!(
                    entry.destroyed,
                    "the helper window must be destroyed before the caller continues: {entry:?}"
                );
                // SAFETY: read-only query about a handle the call already destroyed.
                assert_eq!(
                    unsafe { IsWindow(entry.hwnd) },
                    0,
                    "the destroyed helper window must no longer exist: {entry:?}"
                );
            }
        }

        #[test]
        fn enumeration_error_before_the_helper_window_restores_the_caller_context() {
            if !hardware_or_skip("the pre-window fault-injection check") {
                return;
            }

            for (label, context) in AWARENESS_MODES {
                let Some(caller) = enter_test_context(label, context) else {
                    continue;
                };
                let source = Win32DisplaySource {
                    hooks: ProbeHooks {
                        fault: Some(InjectedFault::BeforeHelperWindow),
                        audit: RefCell::new(Vec::new()),
                    },
                };
                // SAFETY: reads the calling thread's DPI awareness context.
                let before = unsafe { GetThreadDpiAwarenessContext() };
                let result = observe_from(&source);
                // SAFETY: both arguments are valid contexts on this thread.
                let after = unsafe { GetThreadDpiAwarenessContext() };
                let preserved = unsafe { AreDpiAwarenessContextsEqual(after, before) };
                leave_test_context(label, caller);

                let error = result.expect_err("the injected fault must surface as an error");
                assert!(
                    error.message.contains("TEST ONLY fault injection"),
                    "unexpected error: {error}"
                );
                assert_eq!(
                    preserved, 1,
                    "a failed enumeration must still restore the caller's DPI context ({label})"
                );
                assert!(
                    source.hooks.audit.borrow().is_empty(),
                    "no helper window may exist when the failure precedes creation ({label})"
                );
            }
        }

        #[test]
        fn enumeration_error_after_the_helper_window_destroys_it_and_restores_the_caller_context() {
            if !hardware_or_skip("the post-window fault-injection check") {
                return;
            }

            for (label, context) in AWARENESS_MODES {
                let Some(caller) = enter_test_context(label, context) else {
                    continue;
                };
                let source = Win32DisplaySource {
                    hooks: ProbeHooks {
                        fault: Some(InjectedFault::BeforeDpiRead),
                        audit: RefCell::new(Vec::new()),
                    },
                };
                // SAFETY: reads the calling thread's DPI awareness context.
                let before = unsafe { GetThreadDpiAwarenessContext() };
                let result = observe_from(&source);
                // SAFETY: both arguments are valid contexts on this thread.
                let after = unsafe { GetThreadDpiAwarenessContext() };
                let preserved = unsafe { AreDpiAwarenessContextsEqual(after, before) };
                leave_test_context(label, caller);

                let error = result.expect_err("the injected fault must surface as an error");
                assert!(
                    error.message.contains("TEST ONLY fault injection"),
                    "unexpected error: {error}"
                );
                assert_eq!(
                    preserved, 1,
                    "a failed enumeration must still restore the caller's DPI context ({label})"
                );

                let audit = source.hooks.audit.borrow();
                assert!(
                    !audit.is_empty(),
                    "the failure must happen after the helper window existed ({label})"
                );
                for entry in audit.iter() {
                    assert!(
                        entry.destroyed,
                        "the helper window must be destroyed on the error path ({label}): {entry:?}"
                    );
                    // SAFETY: read-only query about a handle the call already destroyed.
                    assert_eq!(
                        unsafe { IsWindow(entry.hwnd) },
                        0,
                        "the helper window must no longer exist after the error ({label}): {entry:?}"
                    );
                }
            }
        }

        #[test]
        fn rectangle_checks_use_current_surface_dimensions_without_rotating_twice() {
            let upright = DisplayMode {
                width: 2560,
                height: 1600,
                quarter_turned: false,
            };
            let rotated = DisplayMode {
                width: 1600,
                height: 2560,
                quarter_turned: true,
            };

            assert!(check_rectangle_against_mode("\\\\.\\DISPLAY1", (2560, 1600), upright).is_ok());
            assert!(check_rectangle_against_mode("\\\\.\\DISPLAY1", (1600, 2560), rotated).is_ok());
            // DEVMODE's current surface dimensions already include rotation.
            // Swapping again would reject this portrait monitor and accept stale
            // landscape geometry during a mode change.
            assert!(
                check_rectangle_against_mode("\\\\.\\DISPLAY1", (2560, 1600), rotated).is_err()
            );
            assert!(
                check_rectangle_against_mode("\\\\.\\DISPLAY1", (1600, 2560), upright).is_err()
            );

            let error = check_rectangle_against_mode("\\\\.\\DISPLAY1", (1707, 1067), upright)
                .expect_err("a rectangle that does not match the mode must be reported");
            assert_eq!(error.code, ErrorCode::SourceLost);
            assert!(error.message.contains("1707x1067"));
            assert!(error.message.contains("2560x1600"));
            assert!(
                !error.message.contains("per-monitor DPI context"),
                "the guard must not diagnose the DPI context: {}",
                error.message
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_rate::NativeRefreshRate;

    /// A synthetic capture selection for the post-initialization comparison tests.
    fn selection() -> CaptureOutput {
        CaptureOutput {
            device_path: "\\\\?\\DISPLAY#SYNTHETIC#1&0&UID0#{synthetic}".to_string(),
            adapter_name: "\\\\.\\SYNTHETIC1".to_string(),
            width: 1920,
            height: 1080,
            is_primary: true,
            refresh_rate_hz: Some(60.0),
            desktop_rect: (0, 0, 1920, 1080),
            monitor_handle: 0x1000,
        }
    }

    #[test]
    fn an_unchanged_resolution_still_matches() {
        let first = selection();
        let mut second = first.clone();
        // Case differences in device and output names are not changes.
        second.device_path = first.device_path.to_uppercase();
        second.adapter_name = first.adapter_name.to_uppercase();
        assert!(first.same_output(&second));
        assert!(first.same_output(&first.clone()));
    }

    #[test]
    fn a_position_only_change_fails_the_recheck() {
        let first = selection();
        let mut moved = first.clone();
        // Same size, same monitor, different desktop position.
        moved.desktop_rect = (0, 1080, 1920, 2160);
        assert!(
            !first.same_output(&moved),
            "a moved display must fail the post-initialization recheck"
        );
    }

    #[test]
    fn a_monitor_only_change_fails_the_recheck() {
        let first = selection();
        let mut reassociated = first.clone();
        // Same identity, name, size and rectangle, different monitor association.
        reassociated.monitor_handle = 0x2000;
        assert!(
            !first.same_output(&reassociated),
            "a different monitor association must fail the post-initialization recheck"
        );
    }

    #[test]
    fn identity_name_and_dimension_changes_fail_the_recheck() {
        let first = selection();
        let mut renamed = first.clone();
        renamed.adapter_name = "\\\\.\\SYNTHETIC2".to_string();
        assert!(!first.same_output(&renamed));

        let mut resized = first.clone();
        resized.height = 1200;
        assert!(!first.same_output(&resized));

        let mut other_display = first.clone();
        other_display.device_path = "\\\\?\\DISPLAY#OTHER#0&0&UID0#{synthetic}".to_string();
        assert!(!first.same_output(&other_display));
    }

    /// Synthetic observation source. Test-only; the production path always uses the
    /// native Windows source.
    struct SyntheticSource {
        observations: Result<NativeObservation, RecorderError>,
    }

    impl DisplayObservationSource for SyntheticSource {
        fn observe(&self) -> Result<NativeObservation, RecorderError> {
            self.observations.clone()
        }
    }

    const DEVICE_PATH: &str =
        "\\\\?\\DISPLAY#BOE0D5E#5&12ac677&0&UID8448#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
    const SECOND_DEVICE_PATH: &str =
        "\\\\?\\DISPLAY#DEL4098#5&2b1a3c4d&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";

    fn monitor(device_path: &str, description: &str, x: i32, y: i32, dpi: u32) -> NativeDisplay {
        NativeDisplay {
            device_path: device_path.to_string(),
            description: description.to_string(),
            adapter_name: "\\\\.\\DISPLAY1".to_string(),
            width: 2560,
            height: 1600,
            x,
            y,
            dpi,
            is_primary: false,
            // Synthetic transient evidence: the rectangle follows the position, and the
            // monitor handle is a fixed test value.
            desktop_rect: (x, y, x + 2560, y + 1600),
            monitor_handle: 0x1000,
        }
    }

    /// A visibly synthetic display-configuration record for one identity.
    fn rate_record(
        display_id: &str,
        adapter_name: &str,
        numerator: u32,
        denominator: u32,
    ) -> ActiveRateRecord {
        ActiveRateRecord {
            source_gdi_device_name: adapter_name.to_string(),
            target_monitor_device_path: display_id.to_string(),
            target_friendly_name: "Synthetic Panel".to_string(),
            refresh: NativeRefreshRate::from_rational(numerator, denominator)
                .expect("synthetic rationals must be convertible"),
            target_available: true,
            output_technology: 11,
            status_flags: 1,
        }
    }

    /// Maps one synthetic list of monitors with one matching rate record each.
    fn map_with_rates(
        monitors: Vec<NativeDisplay>,
        rates: Vec<ActiveRateRecord>,
    ) -> Result<DisplayObservation, RecorderError> {
        map_observation(NativeObservation { monitors, rates })
    }

    /// A synthetic display device record for one adapter output.
    fn device(index: u32, attached: bool, path: &str) -> MonitorDeviceRecord {
        MonitorDeviceRecord {
            index,
            attached_to_desktop: attached,
            mirroring_driver: false,
            interface_path: path.to_string(),
            description: format!("monitor {index}"),
        }
    }

    /// The common single-monitor case: one monitor and its matching record.
    fn map_single(
        monitor: NativeDisplay,
        numerator: u32,
        denominator: u32,
    ) -> Result<DisplayObservation, RecorderError> {
        let record = rate_record(
            &monitor.device_path,
            &monitor.adapter_name,
            numerator,
            denominator,
        );
        map_with_rates(vec![monitor], vec![record])
    }

    fn ids(observation: &DisplayObservation) -> Vec<&str> {
        observation
            .displays
            .iter()
            .map(|display| display.info.id.0.as_str())
            .collect()
    }

    #[test]
    fn identity_is_the_device_interface_path_not_a_session_label() {
        let observation = map_single(
            NativeDisplay {
                is_primary: true,
                ..monitor(DEVICE_PATH, "Generic PnP Monitor", 0, 0, 144)
            },
            144,
            1,
        )
        .expect("must map");
        let displays = observation.displays;

        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].info.id.0, DEVICE_PATH);
        assert_ne!(displays[0].info.id.0, "\\\\.\\DISPLAY1");
        assert_eq!(displays[0].info.name, "Generic PnP Monitor");
        assert!(displays[0].info.is_primary);
        assert_eq!(displays[0].info.width, 2560);
        assert_eq!(displays[0].info.height, 1600);
        // The evidence that came back is the record this display was associated with.
        assert_eq!(displays[0].rate.target_monitor_device_path, DEVICE_PATH);
        assert_eq!(displays[0].rate.source_gdi_device_name, "\\\\.\\DISPLAY1");
    }

    #[test]
    fn scale_factor_is_derived_from_effective_dpi() {
        for (dpi, expected) in [(96u32, 1.0f64), (120, 1.25), (144, 1.5), (192, 2.0)] {
            let observation =
                map_single(monitor("\\\\?\\DISPLAY#A#1", "A", 0, 0, dpi), 60, 1).expect("must map");
            assert_eq!(
                observation.displays[0].info.scale_factor, expected,
                "dpi {dpi}"
            );
        }
    }

    #[test]
    fn a_fractional_mode_rate_reaches_the_wire_value_without_integer_rounding() {
        let observation = map_single(monitor("\\\\?\\DISPLAY#A#1", "A", 0, 0, 144), 60_000, 1_001)
            .expect("must map");
        let info = &observation.displays[0].info;

        assert_ne!(info.refresh_rate_hz, Some(60.0));
        let expected = 60_000.0 / 1_001.0;
        assert_eq!(info.refresh_rate_hz, Some(expected));
        // The private evidence keeps the native rational that produced the value.
        assert_eq!(
            observation.displays[0]
                .rate
                .refresh
                .map(|native| native.to_string()),
            Some("60000/1001".to_string())
        );
        // No HDR capability query exists, so the flag stays conservative.
        assert!(!info.hdr_available);
    }

    #[test]
    fn an_all_zero_native_rate_maps_to_unknown_with_its_evidence_intact() {
        let monitor = monitor("\\\\?\\DISPLAY#A#2", "B", 0, 0, 96);
        let record = ActiveRateRecord {
            refresh: None,
            ..rate_record(&monitor.device_path, &monitor.adapter_name, 60, 1)
        };
        let observation = map_with_rates(vec![monitor], vec![record]).expect("must map");

        assert_eq!(observation.displays[0].info.refresh_rate_hz, None);
        assert!(observation.displays[0].rate.refresh.is_none());
        assert!(!observation.displays[0].info.hdr_available);
    }

    #[test]
    fn order_is_primary_first_then_position_then_identity() {
        let monitors = vec![
            monitor("\\\\?\\DISPLAY#C#3", "C", 2560, 0, 96),
            monitor("\\\\?\\DISPLAY#A#1", "A", -1920, 0, 96),
            NativeDisplay {
                is_primary: true,
                ..monitor("\\\\?\\DISPLAY#B#2", "B", 0, 0, 144)
            },
        ];
        let rates = monitors
            .iter()
            .map(|monitor| rate_record(&monitor.device_path, &monitor.adapter_name, 60, 1))
            .collect();
        let observation = map_with_rates(monitors, rates).expect("must map");

        assert_eq!(
            ids(&observation),
            vec![
                "\\\\?\\DISPLAY#B#2",
                "\\\\?\\DISPLAY#A#1",
                "\\\\?\\DISPLAY#C#3"
            ]
        );
        assert!(observation.displays[0].info.is_primary);
    }

    #[test]
    fn ordering_keeps_each_display_with_its_own_evidence() {
        // The monitor order is reversed relative to the sorted order, so a naive
        // implementation that sorted values and evidence separately would swap them.
        let monitors = vec![
            monitor("\\\\?\\DISPLAY#Z#9", "Z", 2560, 0, 96),
            monitor("\\\\?\\DISPLAY#A#1", "A", -1920, 0, 96),
        ];
        let rates = vec![
            rate_record("\\\\?\\DISPLAY#Z#9", "\\\\.\\DISPLAY1", 165, 1),
            rate_record("\\\\?\\DISPLAY#A#1", "\\\\.\\DISPLAY1", 240, 1),
        ];
        let observation = map_with_rates(monitors, rates).expect("must map");

        assert_eq!(
            ids(&observation),
            vec!["\\\\?\\DISPLAY#A#1", "\\\\?\\DISPLAY#Z#9"]
        );
        assert_eq!(observation.displays[0].info.refresh_rate_hz, Some(240.0));
        assert_eq!(observation.displays[1].info.refresh_rate_hz, Some(165.0));
        assert_eq!(
            observation.displays[0].rate.target_monitor_device_path,
            "\\\\?\\DISPLAY#A#1"
        );
        assert_eq!(
            observation.displays[1].rate.target_monitor_device_path,
            "\\\\?\\DISPLAY#Z#9"
        );
    }

    #[test]
    fn duplicate_native_identity_is_rejected() {
        let monitors = vec![
            monitor("\\\\?\\DISPLAY#A#1", "A", 0, 0, 96),
            monitor("\\\\?\\DISPLAY#A#1", "A", 2560, 0, 96),
        ];
        let rates = vec![rate_record("\\\\?\\DISPLAY#A#1", "\\\\.\\DISPLAY1", 60, 1)];
        let error = map_with_rates(monitors, rates)
            .expect_err("the same display identity must not be reported twice");
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(error.message.contains("more than once"));
    }

    #[test]
    fn empty_native_enumeration_is_a_valid_empty_list() {
        let observations = observe_from(&SyntheticSource {
            observations: Ok(NativeObservation {
                monitors: Vec::new(),
                rates: Vec::new(),
            }),
        })
        .expect("zero monitors is an empty list, not an error");
        assert!(observations.displays.is_empty());
        assert!(observations.into_displays().is_empty());
    }

    #[test]
    fn native_failure_is_reported_instead_of_fabricating_a_display() {
        let error = observe_from(&SyntheticSource {
            observations: Err(source_error(
                "Windows display enumeration failed: EnumDisplayMonitors reported failure."
                    .to_string(),
                "Retry enumeration; if it persists, restart Frame and report the error.",
            )),
        })
        .expect_err("a native failure must not produce displays");

        assert_eq!(error.code, ErrorCode::SourceLost);
        assert_eq!(error.recovery, RecoveryStatus::NoRecording);
        assert!(error.message.contains("EnumDisplayMonitors"));
    }

    #[test]
    fn missing_identity_fails_instead_of_using_a_session_label() {
        let error = map_single(monitor("", "Unnamed", 0, 0, 96), 60, 1)
            .expect_err("a display without stable identity must not be reported");
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(error.message.contains("stable device interface path"));
    }

    #[test]
    fn empty_geometry_and_zero_dpi_fail_explicitly() {
        let mut zero_size = monitor("\\\\?\\DISPLAY#A#1", "A", 0, 0, 96);
        zero_size.width = 0;
        let geometry_error =
            map_single(zero_size, 60, 1).expect_err("empty geometry must not be reported");
        assert_eq!(geometry_error.code, ErrorCode::SourceLost);
        assert!(geometry_error.message.contains("empty rectangle"));

        let dpi_error = map_single(monitor("\\\\?\\DISPLAY#A#1", "A", 0, 0, 0), 60, 1)
            .expect_err("zero DPI must not be reported");
        assert_eq!(dpi_error.code, ErrorCode::SourceLost);
        assert!(dpi_error.message.contains("unusable DPI"));
    }

    #[test]
    fn displays_without_an_os_description_get_a_presentation_ordinal() {
        let monitors = vec![
            monitor("\\\\?\\DISPLAY#A#1", "Named", 2560, 0, 96),
            NativeDisplay {
                is_primary: true,
                ..monitor("\\\\?\\DISPLAY#B#2", "", 0, 0, 144)
            },
        ];
        let rates = monitors
            .iter()
            .map(|monitor| rate_record(&monitor.device_path, &monitor.adapter_name, 60, 1))
            .collect();
        let observation = map_with_rates(monitors, rates).expect("must map");

        assert_eq!(observation.displays[0].info.name, "Display 1");
        assert_eq!(observation.displays[1].info.name, "Named");
        assert_eq!(observation.displays[0].info.id.0, "\\\\?\\DISPLAY#B#2");
    }

    #[test]
    fn a_monitor_without_a_matching_active_path_fails_instead_of_guessing() {
        let monitors = vec![monitor(DEVICE_PATH, "Panel", 0, 0, 144)];
        // The only available record belongs to a different display.
        let rates = vec![rate_record(SECOND_DEVICE_PATH, "\\\\.\\DISPLAY2", 60, 1)];
        let error = map_with_rates(monitors, rates)
            .expect_err("an unmatched display must not borrow another monitor's rate");
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(error.message.contains("no path for display"));
    }

    #[test]
    fn diagnostics_report_keeps_two_different_rates_with_their_own_displays() {
        // Regression coverage for the report: two displays with different rates must
        // not be mixed up, and the evidence printed beside each value must belong to
        // that display.
        let monitors = vec![
            NativeDisplay {
                is_primary: true,
                ..monitor(DEVICE_PATH, "Internal Panel", 0, 0, 144)
            },
            NativeDisplay {
                adapter_name: "\\\\.\\DISPLAY2".to_string(),
                ..monitor(SECOND_DEVICE_PATH, "External Panel", 2560, 0, 96)
            },
        ];
        let rates = vec![
            rate_record(DEVICE_PATH, "\\\\.\\DISPLAY1", 165, 1),
            rate_record(SECOND_DEVICE_PATH, "\\\\.\\DISPLAY2", 60_000, 1_001),
        ];
        let observation = map_with_rates(monitors, rates).expect("must map");
        let report = render_diagnostics(&observation);

        // Both identities are described, each with its own exact rational.
        assert!(report.contains(DEVICE_PATH));
        assert!(report.contains(SECOND_DEVICE_PATH));
        assert!(report.contains("rational=165/1"));
        assert!(report.contains("rational=60000/1001"));
        assert!(report.contains("source=\\\\.\\DISPLAY1"));
        assert!(report.contains("source=\\\\.\\DISPLAY2"));

        // Neither display's line may carry the other's value or evidence.
        let internal_line = report
            .lines()
            .position(|line| line.starts_with("1. Internal Panel"))
            .expect("the primary display must be described first");
        let external_line = report
            .lines()
            .position(|line| line.starts_with("2. External Panel"))
            .expect("the second display must be described");
        assert!(internal_line < external_line);

        let lines: Vec<&str> = report.lines().collect();
        let internal_block = lines[internal_line..external_line].join("\n");
        let external_block = lines[external_line..].join("\n");
        let fractional_value = format!("refresh: {} Hz", 60_000.0 / 1_001.0);
        assert!(internal_block.contains("refresh: 165 Hz"));
        assert!(!internal_block.contains(&fractional_value));
        assert!(external_block.contains(&fractional_value));
        assert!(!external_block.contains("refresh: 165 Hz"));
        assert!(internal_block.contains("rational=165/1"));
        assert!(!internal_block.contains("60000/1001"));
        assert!(internal_block.contains("source=\\\\.\\DISPLAY1"));
        assert!(!internal_block.contains("\\\\.\\DISPLAY2"));

        assert!(external_block.contains("rational=60000/1001"));
        assert!(!external_block.contains("165/1"));
        assert!(external_block.contains("source=\\\\.\\DISPLAY2"));
        assert!(!external_block.contains("\\\\.\\DISPLAY1"));
    }

    #[test]
    fn diagnostics_report_states_what_the_value_is_not() {
        let observation =
            map_single(monitor(DEVICE_PATH, "Panel", 0, 0, 144), 180, 1).expect("must map");
        let report = render_diagnostics(&observation);

        // The report must distinguish the nominal mode rate from capture and encode
        // measurements, and must not promise an atomic snapshot.
        assert!(report.contains("not a capture rate"));
        assert!(report.contains("not an encoded rate"));
        assert!(report.contains("one enumeration pass"));
        assert!(report.contains("not atomic"));
        assert!(report.contains("refresh: 180 Hz"));
        assert!(report.contains("hdr available: no"));
    }

    #[test]
    fn diagnostics_report_describes_a_session_without_displays() {
        let report = render_diagnostics(&DisplayObservation {
            displays: Vec::new(),
        });
        assert!(report.contains("no attached displays"));
        assert!(!report.contains("refresh:"));
    }

    #[test]
    fn monitor_identity_uses_the_documented_index_zero_association() {
        let resolved = resolve_monitor_device(
            "\\\\.\\DISPLAY1",
            &[
                device(0, true, DEVICE_PATH),
                device(1, false, "\\\\?\\DISPLAY#STALE#1"),
            ],
        )
        .expect("index zero is the documented association");
        assert_eq!(resolved.device_path, DEVICE_PATH);
        assert_eq!(resolved.description, "monitor 0");
    }

    #[test]
    fn missing_or_unattached_monitor_devices_are_rejected() {
        let none = resolve_monitor_device("\\\\.\\DISPLAY1", &[])
            .expect_err("no monitor device means no identity");
        assert!(none.message.contains("no desktop-attached monitor device"));

        let unattached =
            resolve_monitor_device("\\\\.\\DISPLAY1", &[device(0, false, DEVICE_PATH)])
                .expect_err("an unattached monitor device means no identity");
        assert!(
            unattached
                .message
                .contains("no desktop-attached monitor device")
        );
    }

    #[test]
    fn ambiguous_monitor_associations_are_rejected_not_guessed() {
        let several = resolve_monitor_device(
            "\\\\.\\DISPLAY1",
            &[
                device(0, true, DEVICE_PATH),
                device(1, true, "\\\\?\\DISPLAY#OTHER#2"),
            ],
        )
        .expect_err("two attached monitor devices are ambiguous");
        assert_eq!(several.code, ErrorCode::SourceLost);
        assert!(several.message.contains("ambiguous"));

        let offset = resolve_monitor_device("\\\\.\\DISPLAY1", &[device(1, true, DEVICE_PATH)])
            .expect_err("the documented association is index zero");
        assert!(offset.message.contains("index zero"));

        let mut many: Vec<MonitorDeviceRecord> = Vec::new();
        for index in 0..=(MAX_MONITOR_DEVICES_PER_ADAPTER as u32) {
            many.push(device(index, false, ""));
        }
        let implausible = resolve_monitor_device("\\\\.\\DISPLAY1", &many)
            .expect_err("an implausible device count is not identity");
        assert!(implausible.message.contains("cannot be trusted"));
    }

    #[test]
    fn mirroring_pseudo_monitors_and_missing_paths_are_rejected() {
        let mirroring = MonitorDeviceRecord {
            mirroring_driver: true,
            ..device(0, true, DEVICE_PATH)
        };
        let error = resolve_monitor_device("\\\\.\\DISPLAY1", &[mirroring])
            .expect_err("a mirroring pseudo-monitor has no display identity");
        assert!(error.message.contains("mirroring"));

        let empty_path = resolve_monitor_device("\\\\.\\DISPLAY1", &[device(0, true, "")])
            .expect_err("an empty interface path is not identity");
        assert!(empty_path.message.contains("interface path"));
    }

    #[test]
    fn cleanup_failures_keep_the_original_enumeration_error() {
        let original = source_error(
            "Windows display enumeration failed: GetMonitorInfoW reported failure.".to_string(),
            "Reconnect the display and enumerate again.",
        );
        let restore = restore_error(
            "the thread DPI awareness context is not the one enumeration started with",
        );
        let merged = merge_restore_failure(original.clone(), restore);
        assert_eq!(merged.code, ErrorCode::SourceLost);
        assert!(merged.message.contains("GetMonitorInfoW"));
        assert!(merged.message.contains("could not be restored"));
        assert!(merged.message.contains("DPI awareness context"));

        let window = window_error("DestroyWindow failed with error 5.".to_string(), "Restart.");
        let merged = merge_window_cleanup_failure(original, window);
        assert_eq!(merged.code, ErrorCode::SourceLost);
        assert!(merged.message.contains("GetMonitorInfoW"));
        assert!(
            merged
                .message
                .contains("helper DPI query window could not be destroyed")
        );
        assert!(merged.message.contains("DestroyWindow failed with error 5"));
    }
}
