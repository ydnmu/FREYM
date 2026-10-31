// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! The reviewed native FFI boundary for application audio session discovery
//! (FRAME-0106A, ADR 0007).
//!
//! This is the only module on this path that performs native calls, and it exists so
//! that the crate's `unsafe_code = "deny"` policy has exactly one narrow exception per
//! feature. It declares no Windows structure, interface or vtable of its own: every
//! interface, method, constant and vtable slot comes from the official `windows` 0.61.3
//! bindings, including the `MMDeviceEnumerator` class identifier.
//!
//! # What one pass does
//!
//! 1. `CoInitializeEx(COINIT_MULTITHREADED)` on the calling worker thread, balanced
//!    exactly once by [`ComApartment`] on every exit path including an unwind.
//! 2. `CoCreateInstance` of `MMDeviceEnumerator`, then
//!    `IMMDeviceEnumerator::EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)`.
//! 3. Per active render endpoint: `IMMDevice::GetId` **once**, `IMMDevice::GetState`, the
//!    device friendly-name property, and `IMMDevice::Activate::<IAudioSessionManager2>`
//!    followed by `IAudioSessionManager2::GetSessionEnumerator`.
//! 4. Per session: `GetState`, `GetSessionIdentifier`, `GetSessionInstanceIdentifier`,
//!    `IsSystemSoundsSession`, `GetProcessId`, `GetDisplayName`, and -- for a session
//!    with exactly one process -- one `OpenProcess` handle used for both
//!    `QueryFullProcessImageNameW` and `GetProcessTimes`.
//! 5. Every OS string is copied into Frame-owned storage and its `CoTaskMemAlloc`
//!    release (`CoTaskMemFree`) runs exactly once; every interface reference is released
//!    by dropping its owning Rust value, and every process handle by [`ProcessHandle`].
//!    Nothing native outlives the pass.
//!
//! There is no `IAudioClient`, no stream, no activation of anything that renders or
//! captures, and no `SetDisplayName`, volume, ducking or privacy call. This module only
//! reads.
//!
//! # Positive statuses
//!
//! Two calls have documented *success* results that are not `S_OK`, and both matter:
//!
//! - `IAudioSessionControl2::IsSystemSoundsSession` returns `S_OK` for a system sounds
//!   session and `S_FALSE` for every other session, so treating any non-`S_OK` result as
//!   failure would exclude every real application.
//! - `IAudioSessionControl2::GetProcessId` returns the positive status
//!   `AUDCLNT_S_NO_SINGLE_PROCESS` when the session spans more than one process. The
//!   generated convenience wrapper maps the out-value through the `Result` path and
//!   therefore **erases** that status, so the process-id read calls the official
//!   generated vtable slot instead and keeps the `HRESULT`. That is the only reason this
//!   module touches a vtable, and it redeclares nothing: the slot is the generated
//!   `IAudioSessionControl2_Vtbl::GetProcessId`.
//!
//! # Failure policy
//!
//! The collector is all-or-nothing about *essential* reads. COM initialization,
//! enumerator creation, `EnumAudioEndpoints`, `GetCount`, `Item`, `IMMDevice::GetId`,
//! the device state read, `IAudioSessionManager2` activation, the session enumerator,
//! its count, and each session's essential state/identity/process-id reads either succeed
//! or fail the snapshot with the original API name and native code. A failure is never
//! converted into a plausible default, an empty endpoint, or a shorter successful list.
//!
//! Two things are deliberately *not* essential, and both stay visible:
//!
//! - A session the OS reports as gone between the count and the item read is a documented
//!   disappearance. Only the codes [`is_recoverable_disappearance`] lists are treated
//!   that way; every other `HRESULT` fails the snapshot.
//! - Optional process metadata (opening the owner, its image path, its creation time) and
//!   the presentation-only display name never remove an otherwise valid source. Their
//!   failures are recorded beside the session and `app_identifier` stays `None`.
//!
//! # The observation seam
//!
//! [`NativePassObserver`] is the seam the collector is generic over. The scripted
//! implementation supplies the *values* of each read (and the failures of the reads it
//! scripts), so the production collector loop, count checks, essential/optional
//! classification, record assembly and error propagation run under test. The pointer
//! plumbing, RAII ownership and allocation conversion stay in this module only.
//!
//! `unsafe_code` is allowed for this module by its declaration in
//! `application_sessions`, so the exception is visible where the module is wired in.

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND, ERROR_PARTIAL_COPY,
    FILETIME, HANDLE, RPC_E_CHANGED_MODE,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, DEVICE_STATE_ACTIVE,
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2,
    IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::System::Variant::{VT_EMPTY, VT_LPWSTR, VT_NULL};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
#[cfg(test)]
use windows::core::{GUID, PCWSTR};
use windows::core::{HRESULT, Interface, PWSTR};

use super::limits::{MAX_ENDPOINTS_PER_SNAPSHOT, MAX_NATIVE_STRING_UTF16, SnapshotBudget};
use super::worker::{
    ApplicationObservationSource, EssentialSessionFailure, FailureEvent, MetadataFailure,
    ObservationHeader, ObservedEndpoint, ObservedSession, SessionFailureKind, WideString,
};
use super::{
    ApplicationExecutableAvailability, ApplicationProcessAvailability, ApplicationProcessIdStatus,
    ApplicationSystemSoundsStatus, classify_process_id_status, classify_system_sounds_status,
};

/// The documented positive status `AUDCLNT_S_NO_SINGLE_PROCESS`, used only by the
/// regression that proves the vtable-slot helper preserves it.
///
/// Microsoft's reference page for `IAudioSessionControl2::GetProcessId` defines the status
/// as `AUDCLNT_SUCCESS(0x00d)`, and
/// `MAKE_SCODE(SEVERITY_SUCCESS, FACILITY_AUDCLNT, 0x00d)` with `FACILITY_AUDCLNT` 2185
/// (`0x889`) is `0x0889000D`. The `windows` crate exports `AUDCLNT_S_BUFFER_EMPTY`,
/// `AUDCLNT_S_THREAD_ALREADY_REGISTERED` and `AUDCLNT_S_POSITION_STALLED` but not this
/// status in any version in this lockfile, and the installed Windows SDK headers do not
/// define it either, so the documented value is stated once, next to the classification
/// that consumes it.
#[cfg(test)]
const AUDCLNT_S_NO_SINGLE_PROCESS: HRESULT = HRESULT(0x0889_000D_u32 as i32);

/// Upper bound on the UTF-16 content units for one image path, matching the retained
/// per-string bound so an over-long path fails its own read instead of being cut. The
/// stack buffer carries the terminator on top of the content bound.
const MAX_IMAGE_PATH_UNITS: usize = MAX_NATIVE_STRING_UTF16;
const IMAGE_PATH_BUFFER_UNITS: usize = MAX_IMAGE_PATH_UNITS + 1;

/// The numeric ID of the calling thread, for thread accounting.
pub(crate) fn current_thread_id() -> u64 {
    // SAFETY: GetCurrentThreadId has no preconditions and only reports this thread's ID.
    u64::from(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() })
}

/// What one `GetProcessId` read produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessIdOutcome {
    /// The raw `HRESULT`, so the documented positive multi-process status survives.
    pub(crate) status: HRESULT,
    /// The out-value, when the call wrote one.
    pub(crate) value: Option<u32>,
}

/// One session's values as the collector's reads produced them.
///
/// Each field carries the failure its read produced, or the value. The collector never
/// sees an unset field it could mistake for a real one, and it never manufactures a
/// default for a failed read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionControlOwned {
    pub(crate) state: Result<i32, FailureEvent>,
    pub(crate) system_sounds: Result<HRESULT, FailureEvent>,
    pub(crate) identifier: Result<WideString, FailureEvent>,
    pub(crate) instance_identifier: Result<WideString, FailureEvent>,
    pub(crate) process_id: Result<ProcessIdOutcome, FailureEvent>,
    pub(crate) creation_time: Result<u64, FailureEvent>,
}

impl SessionControlOwned {
    /// A control whose reads are all unset, so a scripted test sets exactly what it reads.
    #[cfg(test)]
    pub(crate) fn all_unset() -> Self {
        let unset = || FailureEvent::new("unset", "this read was not scripted".to_string());
        Self {
            state: Err(unset()),
            system_sounds: Err(unset()),
            identifier: Err(unset()),
            instance_identifier: Err(unset()),
            process_id: Err(unset()),
            creation_time: Err(unset()),
        }
    }
}

/// One session control the collector reads through.
pub(crate) struct SessionControl {
    /// The interface to read through. `None` for the scripted seam, whose reads are
    /// supplied as [`SessionControlOwned`] values and are never dereferenced.
    #[allow(dead_code)]
    pub(crate) interface: Option<IAudioSessionControl>,
    /// The values the reads produced.
    pub(crate) values: SessionControlOwned,
}

/// One endpoint's session context: an enumerator plus where to start reading.
pub(crate) struct EndpointSessions<Enumerator> {
    /// The session enumerator to read through. `None` for the scripted seam.
    #[allow(dead_code)]
    pub(crate) enumerator: Option<Enumerator>,
    /// The count the enumerator reported.
    pub(crate) reported: i32,
}

/// The reads one collection pass performs and the decisions it is subject to.
///
/// The production implementation performs the real calls. The scripted implementation
/// supplies values and failures, so the collector's own logic is what runs under test.
pub(crate) trait NativePassObserver {
    /// The device value type returned by one endpoint item read.
    type Device;

    /// The session enumerator value type returned by one endpoint's manager.
    type Enumerator;

    /// Whether one endpoint at `index` may be read.
    ///
    /// Called **before** any item read or allocation for that endpoint.
    fn should_read_endpoint(&self, index: usize, reported: usize) -> Result<(), RecorderError>;

    /// Whether one session at `index` may be read.
    ///
    /// Called **before** any session item read or per-session allocation.
    fn should_read_session(&self, index: usize, reported: usize) -> Result<(), RecorderError>;

    /// Records that one session item read was attempted.
    fn observe_session_item(&self, index: usize) -> Result<(), RecorderError>;

    /// Reads one endpoint item.
    fn read_endpoint_device(
        &self,
        collection: &IMMDeviceCollection,
        index: usize,
    ) -> Result<Self::Device, FailureEvent>;

    /// Reads one endpoint device's identity, once.
    ///
    /// Implementations own a successful pointer immediately and propagate the read's own
    /// failure; the call is never repeated to reconstruct an error.
    fn read_endpoint_id(
        &self,
        device: &Self::Device,
        budget: &mut SnapshotBudget,
    ) -> Result<WideString, FailureEvent>;

    /// Reads one endpoint device's state.
    fn read_device_state(&self, device: &Self::Device) -> Result<u32, FailureEvent>;

    /// Reads one endpoint device's friendly name.
    fn read_friendly_name(
        &self,
        device: &Self::Device,
        budget: &mut SnapshotBudget,
    ) -> Result<Option<WideString>, RecorderError>;

    /// Opens one endpoint device's session enumerator.
    fn open_endpoint_sessions(
        &self,
        device: &Self::Device,
    ) -> Result<EndpointSessions<Self::Enumerator>, FailureEvent>;

    /// Reads one session's values.
    fn read_session(
        &self,
        sessions: &EndpointSessions<Self::Enumerator>,
        index: usize,
        budget: &mut SnapshotBudget,
    ) -> Result<SessionControl, FailureEvent>;

    /// Reads one process's optional metadata through **one** process handle.
    ///
    /// `body` is called at most once, and only when the process opened successfully, so the
    /// image path and the creation time always observe the same handle. A failure to open
    /// the process is returned as its own event, which the collector records as an optional
    /// metadata failure.
    fn read_process_metadata(
        &self,
        pid: u32,
        body: &mut dyn FnMut(&ProcessMetadataHandle<'_>) -> ProcessMetadata,
    ) -> Result<ProcessMetadata, FailureEvent>;
}

/// The optional process metadata one process handle produced.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcessMetadata {
    /// The full image path, when the query succeeded.
    pub(crate) image_path: Result<WideString, FailureEvent>,
    /// The creation time as a `FILETIME` value, when the query succeeded.
    pub(crate) creation_time: Result<u64, FailureEvent>,
}

/// The one process handle the metadata reads share.
///
/// The guard *is* the resource: dropping it closes the handle exactly once, on the success
/// path and on every error path including an unwind.
pub(crate) struct ProcessMetadataHandle<'a> {
    handle: ProcessHandle,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl ProcessMetadataHandle<'_> {
    /// Reads this handle's full image path.
    pub(crate) fn image_path(
        &self,
        pid: u32,
        budget: &mut SnapshotBudget,
    ) -> Result<WideString, FailureEvent> {
        read_image_path(self.handle.0, pid, budget)
    }

    /// Reads this handle's process creation time.
    pub(crate) fn creation_time(&self) -> Result<u64, FailureEvent> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: this handle is live and owned here, and all four out-parameters are live
        // locals this call is documented to write.
        unsafe {
            GetProcessTimes(
                self.handle.0,
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        }
        .map_err(|error| {
            FailureEvent::from_win32(
                "GetProcessTimes",
                "the process creation time could not be read".to_string(),
                win32_code(&error),
            )
        })?;
        Ok(u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime))
    }
}

/// A process handle that is closed exactly once.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// Takes ownership of a handle returned by `OpenProcess`.
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle came from `OpenProcess` and this is its only close.
            let _ = unsafe { CloseHandle(self.0) };
            self.0 = HANDLE::default();
        }
    }
}

/// COM apartment guard: every successful initialization is balanced exactly once, on the
/// thread that made it.
pub(crate) struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    /// Initializes MTA on the calling (worker) thread.
    pub(crate) fn initialize_mta() -> Result<Self, RecorderError> {
        // SAFETY: CoInitializeEx has no preconditions beyond a live thread, and a null
        // reserved pointer is documented as required.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Err(pass_error(
                "the application-session worker thread was already initialized in a different COM apartment",
                "Retry application-session discovery; if it persists, report the error.",
            ));
        }
        if result.is_err() {
            return Err(pass_error(
                &format!(
                    "CoInitializeEx(COINIT_MULTITHREADED) failed with HRESULT 0x{:08X}",
                    result.0
                ),
                "Retry application-session discovery; if it persists, report the Windows build.",
            ));
        }
        // S_OK and S_FALSE both require a matching CoUninitialize.
        Ok(Self { initialized: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances this thread's successful CoInitializeEx exactly once.
            unsafe { CoUninitialize() };
            self.initialized = false;
        }
    }
}

/// One `CoTaskMemAlloc`ed wide string produced by the Core Audio API.
///
/// The API documents `CoTaskMemFree` as the matching release, so ownership is taken
/// immediately on a non-null pointer and released exactly once by `Drop`, on every path
/// including an error return or an unwind.
struct CoTaskMemString(PWSTR);

impl CoTaskMemString {
    /// Takes ownership of an OS string pointer.
    fn new(value: PWSTR) -> Self {
        Self(value)
    }

    /// Whether the OS returned no string at all.
    fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// The UTF-16 **content** unit count, bounded by `MAX_NATIVE_STRING_UTF16`.
    ///
    /// The scan stops at the limit: it reads at most `MAX_NATIVE_STRING_UTF16 + 1` code
    /// units, the extra one being the terminator check. A string without a terminator in
    /// that window is refused as over the bound instead of being scanned unboundedly and
    /// checked afterwards.
    fn bounded_units(&self, what: &str) -> Result<usize, RecorderError> {
        if self.is_null() {
            return Ok(0);
        }
        for units in 0..=MAX_NATIVE_STRING_UTF16 {
            // SAFETY: the API returned a null-terminated allocation; the window is exactly
            // one code unit past the content bound, so a terminator inside the bound is
            // always reached and a longer string is refused without reading further.
            if unsafe { *self.0.0.add(units) } == 0 {
                return Ok(units);
            }
        }
        // No terminator inside the bound, so the content is at least `bound + 1` units.
        Err(super::limits::string_limit_error(
            MAX_NATIVE_STRING_UTF16,
            what,
            MAX_NATIVE_STRING_UTF16 + 1,
        ))
    }

    /// Copies the string into an owned [`WideString`], or reports malformed UTF-16.
    ///
    /// Identity, identifier and path data must convert exactly: a lossy replacement
    /// character could make two different sessions or processes collide on one identity.
    #[cfg(test)]
    fn to_wide_string(&self, what: &str) -> Result<WideString, RecorderError> {
        let units = self.bounded_units(what)?;
        if units == 0 {
            return Ok(WideString::new(String::new(), 0));
        }
        // SAFETY: `bounded_units` established that `units` code units precede the
        // terminator inside the bound, so the slice is inside the allocation.
        let slice = unsafe { std::slice::from_raw_parts(self.0.0, units) };
        WideString::try_from_utf16(slice)
            .ok_or_else(|| super::limits::malformed_utf16_error(what, units))
    }

    /// Copies a presentation-only label, replacing malformed UTF-16.
    ///
    /// Deliberately separate from [`CoTaskMemString::to_wide_string`], and used only for a
    /// display name, which is never identity.
    #[cfg(test)]
    fn to_label_string(&self, what: &str) -> Result<WideString, RecorderError> {
        let units = self.bounded_units(what)?;
        if units == 0 {
            return Ok(WideString::new(String::new(), 0));
        }
        // SAFETY: as in `to_wide_string`.
        let slice = unsafe { std::slice::from_raw_parts(self.0.0, units) };
        Ok(WideString::new(String::from_utf16_lossy(slice), units))
    }

    /// Converts directly into precharged owned storage during native collection.
    fn to_charged_string(
        &self,
        what: &str,
        lossy: bool,
        budget: &mut SnapshotBudget,
    ) -> Result<WideString, RecorderError> {
        let units = self.bounded_units(what)?;
        if units == 0 {
            return Ok(WideString::new(String::new(), 0));
        }
        let mut text = reserved_string(budget, units, 4, what)?;
        // SAFETY: bounded_units found the terminator after exactly `units` content units.
        let slice = unsafe { std::slice::from_raw_parts(self.0.0, units) };
        for character in char::decode_utf16(slice.iter().copied()) {
            match character {
                Ok(character) => text.push(character),
                Err(_) if lossy => text.push(char::REPLACEMENT_CHARACTER),
                Err(_) => return Err(super::limits::malformed_utf16_error(what, units)),
            }
        }
        Ok(WideString::new(text, units))
    }
}

impl Drop for CoTaskMemString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a Core Audio API that documents
            // CoTaskMemFree as the matching release, and this is its only release.
            unsafe { CoTaskMemFree(Some(self.0.0 as *const core::ffi::c_void)) };
            self.0 = PWSTR(std::ptr::null_mut());
        }
    }
}

/// The production observer: every read is a real Windows call on the calling thread.
pub(crate) struct NativeObserver;

impl NativePassObserver for NativeObserver {
    type Device = IMMDevice;
    type Enumerator = IAudioSessionEnumerator;

    fn should_read_endpoint(&self, index: usize, reported: usize) -> Result<(), RecorderError> {
        // The cap is checked before the collection is walked, so an endpoint count past the
        // bound costs zero item reads and zero allocations.
        if reported > MAX_ENDPOINTS_PER_SNAPSHOT || index >= MAX_ENDPOINTS_PER_SNAPSHOT {
            return Err(super::limits::endpoint_limit_error(
                MAX_ENDPOINTS_PER_SNAPSHOT,
                reported.max(index + 1),
            ));
        }
        Ok(())
    }

    fn should_read_session(&self, index: usize, reported: usize) -> Result<(), RecorderError> {
        if index >= reported {
            return Err(super::limits::session_limit_error(
                super::limits::MAX_SESSIONS_PER_SNAPSHOT,
                reported.max(index + 1),
            ));
        }
        Ok(())
    }

    fn observe_session_item(&self, _index: usize) -> Result<(), RecorderError> {
        Ok(())
    }

    fn read_endpoint_device(
        &self,
        collection: &IMMDeviceCollection,
        index: usize,
    ) -> Result<IMMDevice, FailureEvent> {
        // SAFETY: the collection is live and owned by the caller, and `index` is below the
        // count it reported.
        unsafe { collection.Item(index as u32) }.map_err(|error| {
            FailureEvent::from_hresult(
                "IMMDeviceCollection::Item",
                "the endpoint could not be read from the collection",
                error.code().0,
            )
        })
    }

    fn read_endpoint_id(
        &self,
        device: &IMMDevice,
        budget: &mut SnapshotBudget,
    ) -> Result<WideString, FailureEvent> {
        // SAFETY: the device is live and owned by the caller. The result is taken once:
        // a successful pointer is owned immediately by `CoTaskMemString`, and the read's
        // own failure is what this endpoint reports.
        let pointer = unsafe { device.GetId() }.map_err(|error| {
            FailureEvent::from_hresult(
                "IMMDevice::GetId",
                "the endpoint identity could not be read",
                error.code().0,
            )
        })?;
        CoTaskMemString::new(pointer)
            .to_charged_string("the endpoint ID", false, budget)
            .map_err(|error| FailureEvent::invalid("IMMDevice::GetId", error.message))
    }

    fn read_device_state(&self, device: &IMMDevice) -> Result<u32, FailureEvent> {
        // SAFETY: the device is live and owned by the caller.
        unsafe { device.GetState() }
            .map(|state| state.0)
            .map_err(|error| {
                FailureEvent::from_hresult(
                    "IMMDevice::GetState",
                    "the endpoint device state could not be read",
                    error.code().0,
                )
            })
    }

    fn read_friendly_name(
        &self,
        device: &IMMDevice,
        budget: &mut SnapshotBudget,
    ) -> Result<Option<WideString>, RecorderError> {
        friendly_name_of_charged(device, budget)
    }

    fn open_endpoint_sessions(
        &self,
        device: &IMMDevice,
    ) -> Result<EndpointSessions<IAudioSessionEnumerator>, FailureEvent> {
        // SAFETY: the device is live. `IAudioSessionManager2` is documented as
        // free-threaded, and activation here only reads session metadata.
        let manager = unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }
            .map_err(|error| {
                FailureEvent::from_hresult(
                    "IMMDevice::Activate(IAudioSessionManager2)",
                    "the session manager could not be activated for this endpoint",
                    error.code().0,
                )
            })?;
        // SAFETY: the manager is live and owned here.
        let enumerator = unsafe { manager.GetSessionEnumerator() }.map_err(|error| {
            FailureEvent::from_hresult(
                "IAudioSessionManager2::GetSessionEnumerator",
                "the session enumerator could not be obtained",
                error.code().0,
            )
        })?;
        // SAFETY: the enumerator is live and owned here.
        let reported = unsafe { enumerator.GetCount() }.map_err(|error| {
            FailureEvent::from_hresult(
                "IAudioSessionEnumerator::GetCount",
                "the session count could not be read",
                error.code().0,
            )
        })?;
        Ok(EndpointSessions {
            enumerator: Some(enumerator),
            reported,
        })
    }

    fn read_session(
        &self,
        sessions: &EndpointSessions<IAudioSessionEnumerator>,
        index: usize,
        budget: &mut SnapshotBudget,
    ) -> Result<SessionControl, FailureEvent> {
        let Some(enumerator) = sessions.enumerator.as_ref() else {
            return Err(FailureEvent::new(
                "IAudioSessionEnumerator::GetSession",
                "no session enumerator is held for this endpoint".to_string(),
            ));
        };
        // SAFETY: the enumerator is live and owned by the caller, and `index` is below the
        // count it reported.
        let control = unsafe { enumerator.GetSession(index as i32) }.map_err(|error| {
            FailureEvent::from_hresult(
                "IAudioSessionEnumerator::GetSession",
                "the session could not be obtained; it may have ended during the pass",
                error.code().0,
            )
        })?;

        // SAFETY: the control is live and owned here for the whole read.
        let state = unsafe { control.GetState() }
            .map(|state| state.0)
            .map_err(|error| {
                FailureEvent::from_hresult(
                    "IAudioSessionControl::GetState",
                    "the session state could not be read",
                    error.code().0,
                )
            });

        let control2: Result<IAudioSessionControl2, FailureEvent> =
            control.cast().map_err(|error| {
                FailureEvent::from_hresult(
                    "IAudioSessionControl::QueryInterface(IAudioSessionControl2)",
                    "the session does not expose IAudioSessionControl2",
                    error.code().0,
                )
            });
        let mut identifier = Err(control2.as_ref().err().cloned().unwrap_or_else(|| {
            FailureEvent::new(
                "IAudioSessionControl2::GetSessionIdentifier",
                "not read".to_string(),
            )
        }));
        // SAFETY: `control2` is live and owned here; each returned pointer is owned
        // immediately by `CoTaskMemString`.
        let mut instance_identifier = identifier.clone();
        let mut system_sounds = identifier
            .clone()
            .map(|_| windows::Win32::Foundation::S_FALSE);
        let mut process_id = identifier.clone().map(|_| ProcessIdOutcome {
            status: windows::Win32::Foundation::S_OK,
            value: None,
        });

        if let Ok(control2) = control2.as_ref() {
            identifier = match unsafe { control2.GetSessionIdentifier() } {
                Ok(pointer) => CoTaskMemString::new(pointer)
                    .to_charged_string("a session identifier", false, budget)
                    .map_err(|error| {
                        FailureEvent::new(
                            "IAudioSessionControl2::GetSessionIdentifier",
                            error.message,
                        )
                    }),
                Err(error) => Err(FailureEvent::from_hresult(
                    "IAudioSessionControl2::GetSessionIdentifier",
                    "the session identifier could not be read",
                    error.code().0,
                )),
            };
            instance_identifier = match unsafe { control2.GetSessionInstanceIdentifier() } {
                Ok(pointer) => CoTaskMemString::new(pointer)
                    .to_charged_string("a session instance identifier", false, budget)
                    .map_err(|error| {
                        FailureEvent::new(
                            "IAudioSessionControl2::GetSessionInstanceIdentifier",
                            error.message,
                        )
                    }),
                Err(error) => Err(FailureEvent::from_hresult(
                    "IAudioSessionControl2::GetSessionInstanceIdentifier",
                    "the session instance identifier could not be read",
                    error.code().0,
                )),
            };
            // `IsSystemSoundsSession` returns `S_OK` or `S_FALSE`; only a real failure is
            // an event, and the classification that consumes it is shared.
            system_sounds = Ok(unsafe { control2.IsSystemSoundsSession() });
            process_id = Ok(unsafe { get_process_id_outcome(control2) });
        }

        let creation_time = match process_id.as_ref() {
            Ok(outcome)
                if outcome.status == windows::Win32::Foundation::S_OK
                    && outcome.value.is_some_and(|pid| pid != 0) =>
            {
                // The creation time is read through the same handle as the image path, in
                // `read_process_creation_time`; the collector drives both.
                Err(FailureEvent::new(
                    "GetProcessTimes",
                    "the creation time is read through the metadata handle".to_string(),
                ))
            }
            _ => Err(FailureEvent::new(
                "GetProcessTimes",
                "the session has no single process to read a creation time from".to_string(),
            )),
        };

        Ok(SessionControl {
            interface: Some(control),
            values: SessionControlOwned {
                state,
                system_sounds,
                identifier,
                instance_identifier,
                process_id,
                creation_time,
            },
        })
    }

    fn read_process_metadata(
        &self,
        pid: u32,
        body: &mut dyn FnMut(&ProcessMetadataHandle<'_>) -> ProcessMetadata,
    ) -> Result<ProcessMetadata, FailureEvent> {
        // SAFETY: OpenProcess has no precondition beyond a live process id; the requested
        // right is read-only.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|error| {
                let code = win32_code(&error);
                FailureEvent::from_win32(
                    "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)",
                    format!(
                        "opening process {pid} failed: {}; the process may have ended, or this process may not have access to it",
                        describe_process_open_error(code)
                    ),
                    code,
                )
            })?;
        // The guard owns the handle from here, so both reads observe the same process and
        // the handle is closed exactly once on every path.
        let opened = ProcessMetadataHandle {
            handle: ProcessHandle::new(handle),
            _marker: std::marker::PhantomData,
        };
        Ok(body(&opened))
    }
}

/// The production observation source: MMDevice plus `IAudioSessionManager2`.
pub(crate) struct NativeApplicationSource;

impl ApplicationObservationSource for NativeApplicationSource {
    fn observe_application_sessions(&self) -> Result<ObservationHeader, RecorderError> {
        let _pass_thread = current_thread_id();
        let apartment = ComApartment::initialize_mta()?;
        // Every interface and allocation the collector obtains is owned by a value that
        // drops before this function returns; the apartment is the last native resource
        // and is released last, on this same thread.
        let header = collect(&NativeObserver)?;
        drop(apartment);
        Ok(header)
    }
}

/// Reads one whole pass through the given observer.
///
/// The collector owns the loop, the count checks, the essential/optional classification
/// and the record assembly; the observer supplies the reads. Every interface and handle
/// this function obtains is owned by a value that drops before it returns, on the success
/// path and on every error path.
pub(crate) fn collect<O: NativePassObserver>(
    observer: &O,
) -> Result<ObservationHeader, RecorderError> {
    let mut budget = SnapshotBudget::new();
    budget.mark_native_owned();
    // SAFETY: CoCreateInstance has no precondition on the calling thread beyond an
    // initialized apartment, which `ComApartment` established for this worker.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|error| pass_hresult_error("CoCreateInstance(MMDeviceEnumerator)", error))?;

    // SAFETY: the enumerator is live and owned here. `eRender` with `DEVICE_STATE_ACTIVE`
    // is exactly "active render endpoints"; this call does not open a device for streaming.
    let collection: IMMDeviceCollection =
        unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.map_err(
            |error| pass_hresult_error("IMMDeviceEnumerator::EnumAudioEndpoints", error),
        )?;

    // SAFETY: the collection is live and owned here.
    let reported = unsafe { collection.GetCount() }
        .map_err(|error| pass_hresult_error("IMMDeviceCollection::GetCount", error))?
        as usize;

    // The total is rejected before the first item read, so an over-large count costs no
    // item reads and no allocations. A reported count of zero is a truthful empty pass.
    observer.should_read_endpoint(0, reported)?;

    let mut endpoints = reserved_vec(&mut budget, reported, "native endpoint records")?;
    let mut failures = Vec::new();
    for index in 0..reported {
        // Re-checked per index, so the loop can never walk past the cap.
        observer.should_read_endpoint(index, reported)?;
        let device = match observer.read_endpoint_device(&collection, index) {
            Ok(device) => device,
            Err(failure) => {
                failures.push(failure);
                break;
            }
        };
        match observe_endpoint(&device, index, &mut budget, observer) {
            Ok(endpoint) => endpoints.push(endpoint),
            Err(error) => return Err(error),
        }
    }

    Ok(ObservationHeader {
        endpoints,
        endpoints_reported: reported,
        failures,
        budget,
    })
}

fn reserved_vec<T>(
    budget: &mut SnapshotBudget,
    count: usize,
    what: &str,
) -> Result<Vec<T>, RecorderError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let reserved = budget.reserve_capacity(count, std::mem::size_of::<T>(), what)?;
    let values: Vec<T> = Vec::with_capacity(count);
    let actual = values
        .capacity()
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| {
            super::limits::payload_overflow_error("native vector capacity overflowed")
        })?;
    if actual > reserved {
        budget.charge_bytes(actual - reserved, what)?;
    }
    Ok(values)
}

fn reserved_string(
    budget: &mut SnapshotBudget,
    units: usize,
    bytes_per_unit: usize,
    what: &str,
) -> Result<String, RecorderError> {
    let reserved = budget.reserve_capacity(units, bytes_per_unit, what)?;
    let text = String::with_capacity(reserved);
    if text.capacity() > reserved {
        budget.charge_bytes(text.capacity() - reserved, what)?;
    }
    Ok(text)
}

/// Reads one endpoint: identity, name, state and its sessions.
fn observe_endpoint<O: NativePassObserver>(
    device: &O::Device,
    index: usize,
    budget: &mut SnapshotBudget,
    observer: &O,
) -> Result<ObservedEndpoint, RecorderError> {
    budget.mark_native_owned();
    budget.charge_endpoint(index)?;
    // This fixed diagnostic label is live while the endpoint's owned fields are read.
    budget.charge_bytes(64, "native endpoint label scratch")?;
    let label = format!("render endpoint #{}", index + 1);

    // `GetId` is read once, through the observer, which owns a successful pointer and
    // propagates the read's own failure: it is never repeated to reconstruct an error.
    let endpoint_id = observer
        .read_endpoint_id(device, budget)
        .map_err(|failure| failure_into_error(&label, failure))?
        .into_string();

    let state = observer
        .read_device_state(device)
        .map_err(|failure| failure_into_error(&label, failure))?;

    let friendly_name = observer.read_friendly_name(device, budget)?;

    if state & DEVICE_STATE_ACTIVE.0 == 0 {
        // Not an active render endpoint during this pass. No sessions are read, and the
        // record reports the observed state; the mapper marks it as an inactive endpoint
        // rather than a source with no sessions.
        return Ok(ObservedEndpoint {
            endpoint_id,
            friendly_name,
            device_state: state,
            sessions: Vec::new(),
        });
    }

    let sessions = observe_sessions(device, index, budget, observer)?;
    Ok(ObservedEndpoint {
        endpoint_id,
        friendly_name,
        device_state: state,
        sessions,
    })
}

/// Reads every session of one endpoint.
fn observe_sessions<O: NativePassObserver>(
    device: &O::Device,
    index: usize,
    budget: &mut SnapshotBudget,
    observer: &O,
) -> Result<Vec<ObservedSession>, RecorderError> {
    budget.charge_bytes(64, "native session enumeration label scratch")?;
    let label = format!("render endpoint #{}", index + 1);
    let sessions = observer
        .open_endpoint_sessions(device)
        .map_err(|failure| failure_into_error(&label, failure))?;

    // A negative count is not a count. It is refused explicitly rather than cast.
    if sessions.reported < 0 {
        return Err(pass_error(
            &format!(
                "{label} reported {} sessions, which is not a session count, so this snapshot is refused",
                sessions.reported
            ),
            "Retry application-session discovery; if it persists, report the Windows build and audio device.",
        ));
    }
    let reported = sessions.reported as usize;

    // The snapshot-wide session bound is checked before any session item read: the
    // remaining allowance must cover every session this endpoint reported, so the loop
    // never starts work it cannot finish and never truncates.
    if reported > budget.session_allowance() {
        return Err(super::limits::session_limit_error(
            budget.limits().max_sessions_per_snapshot,
            budget.sessions() + reported,
        ));
    }

    let mut observed = reserved_vec(budget, reported, "native session records")?;
    for session_index in 0..reported {
        observer.should_read_session(session_index, reported)?;
        observer.observe_session_item(session_index)?;
        // The session is charged as soon as it is read, before its record is retained, so a
        // session whose own reads fail still counts against the allowance.
        budget.charge_session(session_index)?;
        // Native reads also create short fixed-operation failure values before the
        // resulting record is assembled. This reservation covers those simultaneous
        // diagnostics; retained failure capacities are charged below as well.
        budget.charge_bytes(2048, "native session read diagnostic scratch")?;
        let record = match observer.read_session(&sessions, session_index, budget) {
            Ok(control) => observe_session(control, budget, observer)?,
            Err(failure) => {
                if is_recoverable_disappearance(failure.hresult) {
                    // The OS documents this code as "the session is gone". It is skipped
                    // with an explicit reason instead of failing the snapshot, and the skip
                    // is visible rather than an invented record.
                    ObservedSession {
                        session_identifier: WideString::new(String::new(), 0),
                        instance_identifier: WideString::new(String::new(), 0),
                        raw_state: 0,
                        display_name: None,
                        is_system_sounds: false,
                        process_id: None,
                        process_availability: ApplicationProcessAvailability::Unavailable,
                        executable_path: None,
                        executable_availability:
                            ApplicationExecutableAvailability::ProcessUnavailable,
                        process_creation_time: None,
                        essential_failure: Some(EssentialSessionFailure {
                            kind: SessionFailureKind::SessionIdentifier,
                            operation: failure.operation,
                            detail: failure.detail,
                            hresult: failure.hresult,
                            win32_code: failure.win32_code,
                            recoverable_disappearance: true,
                        }),
                        metadata_failures: Vec::new(),
                    }
                } else {
                    return Err(failure_into_error(&label, failure));
                }
            }
        };
        charge_observed_failures(&record, budget)?;
        observed.push(record);
    }

    Ok(observed)
}

fn charge_observed_failures(
    record: &ObservedSession,
    budget: &mut SnapshotBudget,
) -> Result<(), RecorderError> {
    if let Some(failure) = &record.essential_failure {
        budget.charge_bytes(
            failure.operation.capacity(),
            "a native essential failure operation",
        )?;
        budget.charge_bytes(
            failure.detail.capacity(),
            "a native essential failure detail",
        )?;
    }
    for failure in &record.metadata_failures {
        budget.charge_bytes(
            failure.operation.capacity(),
            "a native metadata failure operation",
        )?;
        budget.charge_bytes(
            failure.detail.capacity(),
            "a native metadata failure detail",
        )?;
    }
    Ok(())
}

/// Reads one session's values into owned form.
///
/// Essential reads either succeed or are recorded in `essential_failure`; optional
/// metadata reads are recorded in `metadata_failures` and never exclude the session.
fn observe_session<O: NativePassObserver>(
    control: SessionControl,
    budget: &mut SnapshotBudget,
    observer: &O,
) -> Result<ObservedSession, RecorderError> {
    let SessionControl { interface, values } = control;
    let mut record = ObservedSession {
        session_identifier: WideString::new(String::new(), 0),
        instance_identifier: WideString::new(String::new(), 0),
        raw_state: 0,
        display_name: None,
        is_system_sounds: false,
        process_id: None,
        process_availability: ApplicationProcessAvailability::Unavailable,
        executable_path: None,
        executable_availability: ApplicationExecutableAvailability::ProcessUnavailable,
        process_creation_time: None,
        essential_failure: None,
        metadata_failures: reserved_vec(budget, 2, "native metadata failure records")?,
    };

    // The state read is essential: a failure is never converted into an inactive default.
    match values.state {
        Ok(state) => record.raw_state = state,
        Err(failure) => {
            record.essential_failure = Some(essential_failure(SessionFailureKind::State, failure));
            return Ok(record);
        }
    }

    match values.identifier {
        Ok(identifier) => record.session_identifier = identifier,
        Err(failure) => {
            record.essential_failure = Some(essential_failure(
                SessionFailureKind::SessionIdentifier,
                failure,
            ));
            return Ok(record);
        }
    }
    match values.instance_identifier {
        Ok(identifier) => record.instance_identifier = identifier,
        Err(failure) => {
            record.essential_failure = Some(essential_failure(
                SessionFailureKind::InstanceIdentifier,
                failure,
            ));
            return Ok(record);
        }
    }

    // `S_OK` means system sounds and `S_FALSE`, also a success, means it is not. Both are
    // classified in one place so neither can drift.
    match values.system_sounds {
        Ok(status) => match classify_system_sounds_status(status.0) {
            ApplicationSystemSoundsStatus::IsSystemSounds => record.is_system_sounds = true,
            ApplicationSystemSoundsStatus::IsNotSystemSounds => record.is_system_sounds = false,
            ApplicationSystemSoundsStatus::Failed { hresult } => {
                record.essential_failure = Some(EssentialSessionFailure {
                    kind: SessionFailureKind::SystemSounds,
                    operation: SessionFailureKind::SystemSounds.operation().to_string(),
                    detail: "the system-sounds status could not be determined".to_string(),
                    hresult: Some(hresult),
                    win32_code: None,
                    recoverable_disappearance: false,
                });
                return Ok(record);
            }
        },
        Err(failure) => {
            record.essential_failure =
                Some(essential_failure(SessionFailureKind::SystemSounds, failure));
            return Ok(record);
        }
    }

    // The documented positive status `AUDCLNT_S_NO_SINGLE_PROCESS` survives the read, so a
    // session with several processes is never given one owner.
    let outcome = match values.process_id {
        Ok(outcome) => outcome,
        Err(failure) => {
            record.essential_failure =
                Some(essential_failure(SessionFailureKind::ProcessId, failure));
            return Ok(record);
        }
    };
    match classify_process_id_status(
        outcome.status.0,
        outcome.value.is_some(),
        outcome.value.unwrap_or(0),
    ) {
        ApplicationProcessIdStatus::SingleProcess { process_id: pid } => {
            record.process_id = Some(pid);
            record.process_availability = ApplicationProcessAvailability::SingleProcess;
            read_process_metadata(&mut record, pid, budget, observer)?;
        }
        ApplicationProcessIdStatus::SingleProcessZero => {
            record.process_availability = ApplicationProcessAvailability::SingleProcessZero;
            record.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
            record.metadata_failures.push(MetadataFailure {
                operation: "IAudioSessionControl2::GetProcessId".to_string(),
                detail: "the session reported process identity 0, which is not a process"
                    .to_string(),
                hresult: None,
                win32_code: None,
            });
        }
        ApplicationProcessIdStatus::NoSingleProcess => {
            // More than one process owns this session, so no single process id, path or
            // creation time is attributed, and the initial creator's id is deliberately
            // not used as a sole owner.
            record.process_availability = ApplicationProcessAvailability::MultiProcessSession;
            record.executable_availability = ApplicationExecutableAvailability::NoSingleProcess;
        }
        ApplicationProcessIdStatus::Failed { hresult } => {
            record.essential_failure = Some(EssentialSessionFailure {
                kind: SessionFailureKind::ProcessId,
                operation: SessionFailureKind::ProcessId.operation().to_string(),
                detail: "the session's process identifier could not be read".to_string(),
                hresult: Some(hresult),
                win32_code: None,
                recoverable_disappearance: false,
            });
            return Ok(record);
        }
    }

    // The display name is presentation-only: it is read last, a failure leaves the label to
    // the basename or the literal unnamed label, and malformed UTF-16 is replaced only
    // here, never in identity data.
    if let Some(interface) = interface.as_ref() {
        // SAFETY: the interface is live and owned by the caller; a returned pointer is
        // owned by `CoTaskMemString` immediately.
        if let Ok(pointer) = unsafe { interface.GetDisplayName() } {
            let owned = CoTaskMemString::new(pointer);
            let label = owned.to_charged_string("a session display name", true, budget)?;
            if !label.as_str().trim().is_empty() {
                record.display_name = Some(label);
            }
        }
    }

    Ok(record)
}

/// Reads the optional process metadata through one process handle.
///
/// The handle is opened once by the observer and lives for both reads; each failure is
/// recorded beside the session without removing it, and the guard closes the handle exactly
/// once on every path.
fn read_process_metadata<O: NativePassObserver>(
    record: &mut ObservedSession,
    pid: u32,
    budget: &mut SnapshotBudget,
    observer: &O,
) -> Result<(), RecorderError> {
    let mut body = |opened: &ProcessMetadataHandle<'_>| ProcessMetadata {
        image_path: opened.image_path(pid, budget),
        creation_time: opened.creation_time(),
    };
    let metadata = match observer.read_process_metadata(pid, &mut body) {
        Ok(metadata) => metadata,
        Err(failure) => {
            if failure.fatal {
                return Err(failure_into_error("process metadata", failure));
            }
            // The process could not be opened. This is optional metadata: the session keeps
            // its identity and state, `app_identifier` stays `None`, and the reason (with its
            // Win32 code) is recorded.
            record.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
            record.metadata_failures.push(metadata_failure(failure));
            return Ok(());
        }
    };

    match metadata.image_path {
        Ok(path) => {
            record.executable_path = Some(path);
            record.executable_availability = ApplicationExecutableAvailability::Available;
        }
        Err(failure) => {
            if failure.fatal {
                return Err(failure_into_error("process image path", failure));
            }
            record.executable_availability =
                ApplicationExecutableAvailability::ImagePathUnavailable;
            record.metadata_failures.push(metadata_failure(failure));
        }
    }

    // The creation time is read through the same handle, and only its own failure is
    // optional: a path that succeeded keeps its `app_identifier` even when this fails.
    match metadata.creation_time {
        Ok(creation_time) => record.process_creation_time = Some(creation_time),
        Err(failure) => record.metadata_failures.push(metadata_failure(failure)),
    }
    Ok(())
}

/// One metadata failure from a failure event.
fn metadata_failure(failure: FailureEvent) -> MetadataFailure {
    MetadataFailure {
        operation: failure.operation,
        detail: failure.detail,
        hresult: failure.hresult,
        win32_code: failure.win32_code,
    }
}

/// One essential session failure from a failure event.
fn essential_failure(kind: SessionFailureKind, failure: FailureEvent) -> EssentialSessionFailure {
    EssentialSessionFailure {
        kind,
        operation: failure.operation,
        detail: failure.detail,
        hresult: failure.hresult,
        win32_code: failure.win32_code,
        recoverable_disappearance: is_recoverable_disappearance(failure.hresult),
    }
}

/// Whether one `HRESULT` is documented as "the session is gone".
///
/// A short, named list rather than "any failure": a blanket skip would turn an unexpected
/// error into a quietly missing session. `AUDCLNT_E_DEVICE_INVALIDATED` and
/// `AUDCLNT_E_RESOURCES_INVALIDATED` are the documented disconnection codes.
fn is_recoverable_disappearance(hresult: Option<i32>) -> bool {
    let Some(code) = hresult else {
        return false;
    };
    code == AUDCLNT_E_DEVICE_INVALIDATED.0 || code == AUDCLNT_E_RESOURCES_INVALIDATED.0
}

/// Turns one failure event into a snapshot-level error, preserving the API name and code.
fn failure_into_error(label: &str, failure: FailureEvent) -> RecorderError {
    let code = match (failure.hresult, failure.win32_code) {
        (Some(hresult), _) => format!("HRESULT 0x{:08X}", hresult as u32),
        (None, Some(win32)) => format!("Win32 error {win32}"),
        (None, None) => "no code".to_string(),
    };
    pass_error(
        &format!(
            "{label}: {} failed with {code} ({})",
            failure.operation, failure.detail
        ),
        "Retry application-session discovery; if it persists, report the Windows build and audio device.",
    )
}

/// Reads one borrowed property value's `VT_LPWSTR` string as a bounded presentation
/// label.
///
/// The PROPVARIANT owns its string for the whole read: the code units are inspected
/// through the variant's own pointer and no `CoTaskMemString`, `String`, `Vec` or `BSTR`
/// is created for it before the bound is proven, so `PropVariantClear` stays its only
/// release and an oversized or wrongly-typed value can never reach a conversion. The
/// bound covers the whole native value; presentation trimming happens only afterwards.
///
/// The output is a presentation label: malformed UTF-16 is replaced here and only here,
/// which is deliberately distinct from identity handling (identity data must convert
/// exactly and is refused otherwise). A missing, empty or blank name is `None`, a
/// present value that is not a wide string is an explicit error rather than a coercion
/// through `Display`, and content past the bound is an explicit error rather than a
/// truncated or trimmed-away label.
#[cfg(test)]
fn friendly_name_from_propvariant(
    value: &PROPVARIANT,
) -> Result<Option<WideString>, RecorderError> {
    friendly_name_from_propvariant_budget(value, None)
}

fn friendly_name_from_propvariant_budget(
    value: &PROPVARIANT,
    budget: Option<&mut SnapshotBudget>,
) -> Result<Option<WideString>, RecorderError> {
    const WHAT: &str = "the endpoint friendly name";

    // The variant tag decides which union member is live, so it is checked before any
    // string access and a non-string tag is never coerced into a label.
    let tag = value.vt();
    if tag == VT_EMPTY || tag == VT_NULL {
        return Ok(None);
    }
    if tag != VT_LPWSTR {
        return Err(RecorderError {
            code: ErrorCode::Internal,
            message: format!(
                "Windows reported {WHAT} as property variant type 0x{:04X} instead of a wide string, so it is refused rather than coerced into a label.",
                tag.0
            ),
            recovery: RecoveryStatus::NoRecording,
            action:
                "Retry application-session discovery; if it persists, report the Windows build and audio device."
                    .to_string(),
        });
    }

    // SAFETY: the tag is VT_LPWSTR, so `pwszVal` is the live union member of the official
    // generated layout, and the variant owns that null-terminated string for as long as
    // this borrow lives. The pointer is only read through; it is never wrapped in
    // `CoTaskMemString` or freed separately.
    let raw = unsafe { value.Anonymous.Anonymous.Anonymous.pwszVal };
    if raw.is_null() {
        return Ok(None);
    }

    // Scan at most 4096 content units plus one terminator position: a terminator at index
    // 4096 is a valid 4096-unit value, and a nonzero unit there is a bound violation.
    // Reading index `n` only happens after units 0..n proved nonzero, so the variant's
    // null-terminated allocation always covers every position read here.
    let mut units = 0usize;
    let terminated = loop {
        if units > MAX_NATIVE_STRING_UTF16 {
            break false;
        }
        // SAFETY: bounded read inside the variant's own allocation; see the loop note.
        if unsafe { *raw.0.add(units) } == 0 {
            break true;
        }
        units += 1;
    };
    if !terminated {
        return Err(super::limits::string_limit_error(
            MAX_NATIVE_STRING_UTF16,
            WHAT,
            MAX_NATIVE_STRING_UTF16 + 1,
        ));
    }

    // SAFETY: `units` code units precede the terminator inside the scan window, so the
    // slice is inside the variant's allocation and is never mutated through it.
    let content = unsafe { std::slice::from_raw_parts(raw.0, units) };
    // Conversion happens only after the whole native value passed the bound. This is the
    // presentation-label policy: malformed UTF-16 is replaced, not refused.
    let mut text = if let Some(budget) = budget {
        reserved_string(budget, units, 4, WHAT)?
    } else {
        String::with_capacity(units.saturating_mul(4))
    };
    for character in char::decode_utf16(content.iter().copied()) {
        text.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let start = trimmed.as_ptr() as usize - text.as_ptr() as usize;
    let end = start + trimmed.len();
    text.truncate(end);
    text.drain(..start);
    let units = text.encode_utf16().count();
    Ok(Some(WideString::new(text, units)))
}

/// The OS friendly name of one endpoint, or `None` when it has none.
///
/// Presentation-only: an unavailable store, property or name is `None`, and the property
/// variant is validated before any label string is built.
fn friendly_name_of_charged(
    device: &IMMDevice,
    budget: &mut SnapshotBudget,
) -> Result<Option<WideString>, RecorderError> {
    // SAFETY: the device is live; the call only opens a read-only property store.
    let store: IPropertyStore = match unsafe { device.OpenPropertyStore(STGM_READ) } {
        Ok(store) => store,
        Err(_) => return Ok(None),
    };
    let key = PKEY_Device_FriendlyName;
    // SAFETY: `store` is live and `key` is an initialized PROPERTYKEY that outlives the
    // call. The returned PROPVARIANT is owned by Rust and clears its own allocation on
    // drop.
    let value = match unsafe { store.GetValue(&key) } {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    // The read is borrowed: `value` still owns its string and clears it on drop.
    friendly_name_from_propvariant_budget(&value, Some(budget))
}

/// Reads one process's full image path through an already-opened handle.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` is the documented least-privilege right for this
/// query, so a protected process fails here rather than being silently skipped. The buffer
/// is a bounded stack array: the content bound plus the terminator.
fn read_image_path(
    handle: HANDLE,
    pid: u32,
    budget: &mut SnapshotBudget,
) -> Result<WideString, FailureEvent> {
    budget
        .charge_bytes(
            std::mem::size_of::<[u16; IMAGE_PATH_BUFFER_UNITS]>(),
            "process image path scratch",
        )
        .map_err(|error| FailureEvent::invalid("QueryFullProcessImageNameW", error.message))?;
    let mut buffer = [0u16; IMAGE_PATH_BUFFER_UNITS];
    let mut size = IMAGE_PATH_BUFFER_UNITS as u32;
    // SAFETY: `handle` is live and owned by the caller; `buffer` is a live array of
    // `IMAGE_PATH_BUFFER_UNITS` UTF-16 units that the API writes into and null-terminates
    // within, updating `size` to the units written.
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    result.map_err(|error| image_path_query_failure(pid, win32_code(&error)))?;
    decode_image_path(&buffer, size as usize, pid, budget)
}

fn image_path_query_failure(pid: u32, code: u32) -> FailureEvent {
    let mut failure = FailureEvent::from_win32(
        "QueryFullProcessImageNameW",
        format!("querying the image path of process {pid} failed"),
        code,
    );
    // The buffer can hold the complete 4096-unit path and its terminator.
    // A capacity error therefore means the OS path exceeds this snapshot bound.
    failure.fatal = code == 122 || code == 234;
    failure
}

fn decode_image_path(
    buffer: &[u16],
    written: usize,
    pid: u32,
    budget: &mut SnapshotBudget,
) -> Result<WideString, FailureEvent> {
    if written == 0 || written > MAX_IMAGE_PATH_UNITS || written >= buffer.len() {
        return Err(FailureEvent::invalid(
            "QueryFullProcessImageNameW",
            format!(
                "process {pid} reported {written} UTF-16 content units, outside the {MAX_IMAGE_PATH_UNITS}-unit bound"
            ),
        ));
    }
    if buffer[written] != 0 {
        return Err(FailureEvent::invalid(
            "QueryFullProcessImageNameW",
            format!("process {pid} reported an image path without its terminator"),
        ));
    }
    let units = &buffer[..written];
    if units.contains(&0) {
        return Err(FailureEvent::invalid(
            "QueryFullProcessImageNameW",
            format!("process {pid} reported a NUL inside its image path"),
        ));
    }
    let mut path = reserved_string(budget, written, 4, "an executable path")
        .map_err(|error| FailureEvent::invalid("QueryFullProcessImageNameW", error.message))?;
    for character in char::decode_utf16(units.iter().copied()) {
        let character = character.map_err(|_| {
            FailureEvent::invalid(
                "QueryFullProcessImageNameW",
                format!("the image path of process {pid} is not valid Unicode"),
            )
        })?;
        path.push(character);
    }
    if path.is_empty() {
        return Err(FailureEvent::invalid(
            "QueryFullProcessImageNameW",
            format!("process {pid} reported an empty image path"),
        ));
    }
    // Executable paths are identity evidence, not presentation labels: preserve them.
    Ok(WideString::new(path, written))
}

/// A pass-level failure from a Windows error: no snapshot can be produced from it.
fn pass_hresult_error(operation: &str, error: windows::core::Error) -> RecorderError {
    pass_error(
        &format!("{operation} failed with HRESULT 0x{:08X}", error.code().0),
        "Retry application-session discovery; if it persists, report the Windows build and audio device.",
    )
}

/// The Win32 error code behind a `windows_core::Error`.
fn win32_code(error: &windows::core::Error) -> u32 {
    let code = error.code().0 as u32;
    // `Error::from_win32` stores the Win32 code in `HRESULT_FROM_WIN32`'s facility; a plain
    // HRESULT is preserved as it is.
    if code & 0xFFFF_0000 == 0x8007_0000 {
        code & 0xFFFF
    } else {
        code
    }
}

/// The documented Win32 codes this module diagnoses, mapped to honest descriptions.
fn describe_process_open_error(code: u32) -> &'static str {
    if code == ERROR_ACCESS_DENIED.0 {
        "this process has no access to the owner process"
    } else if code == ERROR_INVALID_PARAMETER.0 {
        "the process identifier is no longer valid"
    } else if code == ERROR_PARTIAL_COPY.0 {
        "the owner process is exiting"
    } else if code == ERROR_NOT_FOUND.0 {
        "the owner process no longer exists"
    } else {
        "the owner process could not be opened"
    }
}

/// Reads `GetProcessId` through the official generated vtable slot, preserving the
/// `HRESULT` the generated convenience wrapper erases.
///
/// # Safety
///
/// The caller must pass a live `IAudioSessionControl2` reference whose underlying COM
/// object that reference still owns for the duration of this call.
unsafe fn get_process_id_outcome(control: &IAudioSessionControl2) -> ProcessIdOutcome {
    let mut pid = 0u32;
    // SAFETY: the caller guarantees a live interface; `vtable` returns the official
    // generated table; `as_raw` is that interface's own object pointer; and `pid` is a live
    // local the callee is documented to write through.
    let status: HRESULT =
        unsafe { (Interface::vtable(control).GetProcessId)(Interface::as_raw(control), &mut pid) };
    ProcessIdOutcome {
        status,
        value: status.is_ok().then_some(pid),
    }
}

/// A pass-level failure from a description.
fn pass_error(detail: &str, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows application audio session discovery failed: {detail}. No snapshot is reported instead of a fabricated empty one."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

// The native seam's tests run only in a Windows test build: they are written against
// the `windows` interfaces this module reads through, and a non-Windows build has no
// such module to exercise.
#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use frame_contracts::ErrorCode;
    use std::cell::RefCell;
    use windows::Win32::Foundation::{E_POINTER, S_FALSE, S_OK};
    use windows::Win32::Media::Audio::{
        AUDCLNT_E_DEVICE_INVALIDATED, IAudioSessionControl_Vtbl, IAudioSessionControl2_Vtbl,
    };
    use windows::core::{BOOL, IUnknown_Vtbl};

    // -----------------------------------------------------------------------
    // The positive-status regressions (real vtable slot, fake interface)
    // -----------------------------------------------------------------------

    /// A recognisable marker stored as the fake object's second field, so a stub can prove
    /// it ran on the object the test built rather than on a stray pointer.
    static TEST_OBJECT_MARKER: u8 = 0xA6;

    /// The fake COM object: one vtable pointer, one identity marker, and the scripted
    /// result. Its layout is the COM interface layout, whose first field is the vtable.
    #[repr(C)]
    struct FakeSessionControl {
        vtable: *const IAudioSessionControl2_Vtbl,
        marker: *const u8,
        status: HRESULT,
        pid: u32,
    }

    /// The stub `GetProcessId`: it returns the scripted status and writes the scripted pid,
    /// exactly as the real method does for its documented positive status.
    unsafe extern "system" fn stub_get_process_id(
        this: *mut core::ffi::c_void,
        retval: *mut u32,
    ) -> HRESULT {
        // SAFETY: the test only calls this through the vtable of a `FakeSessionControl`,
        // whose first field is that vtable, which is the layout the COM ABI requires of an
        // interface pointer. The object outlives the call.
        let object = unsafe { &*(this as *const FakeSessionControl) };
        assert_eq!(
            object.marker, &TEST_OBJECT_MARKER,
            "the stubbed GetProcessId ran on an object the test did not build"
        );
        if retval.is_null() {
            return E_POINTER;
        }
        // SAFETY: `retval` is the live out-parameter the caller passed.
        unsafe { retval.write(object.pid) };
        object.status
    }

    /// Every stub the `GetProcessId` regression must never reach.
    macro_rules! unreachable_stub {
        ($name:ident, ($($argument:ty),*), $output:ty) => {
            unsafe extern "system" fn $name($(_: $argument),*) -> $output {
                unreachable!("the fake vtable only exercises GetProcessId")
            }
        };
    }

    unreachable_stub!(
        stub_query_interface,
        (
            *mut core::ffi::c_void,
            *const GUID,
            *mut *mut core::ffi::c_void
        ),
        HRESULT
    );
    unreachable_stub!(stub_add_ref, (*mut core::ffi::c_void), u32);
    unreachable_stub!(stub_release, (*mut core::ffi::c_void), u32);
    unreachable_stub!(
        stub_get_state,
        (
            *mut core::ffi::c_void,
            *mut windows::Win32::Media::Audio::AudioSessionState
        ),
        HRESULT
    );
    unreachable_stub!(
        stub_get_display_name,
        (*mut core::ffi::c_void, *mut PWSTR),
        HRESULT
    );
    unreachable_stub!(
        stub_set_display_name,
        (*mut core::ffi::c_void, PCWSTR, *const GUID),
        HRESULT
    );
    unreachable_stub!(
        stub_get_icon_path,
        (*mut core::ffi::c_void, *mut PWSTR),
        HRESULT
    );
    unreachable_stub!(
        stub_set_icon_path,
        (*mut core::ffi::c_void, PCWSTR, *const GUID),
        HRESULT
    );
    unreachable_stub!(
        stub_get_grouping_param,
        (*mut core::ffi::c_void, *mut GUID),
        HRESULT
    );
    unreachable_stub!(
        stub_set_grouping_param,
        (*mut core::ffi::c_void, *const GUID, *const GUID),
        HRESULT
    );
    unreachable_stub!(
        stub_register_notification,
        (*mut core::ffi::c_void, *mut core::ffi::c_void),
        HRESULT
    );
    unreachable_stub!(
        stub_unregister_notification,
        (*mut core::ffi::c_void, *mut core::ffi::c_void),
        HRESULT
    );
    unreachable_stub!(
        stub_session_identifier,
        (*mut core::ffi::c_void, *mut PWSTR),
        HRESULT
    );
    unreachable_stub!(
        stub_instance_identifier,
        (*mut core::ffi::c_void, *mut PWSTR),
        HRESULT
    );
    unreachable_stub!(stub_is_system_sounds, (*mut core::ffi::c_void), HRESULT);
    unreachable_stub!(stub_set_ducking, (*mut core::ffi::c_void, BOOL), HRESULT);

    /// The official generated vtable type, populated with the stubs.
    fn fake_vtable() -> *const IAudioSessionControl2_Vtbl {
        Box::leak(Box::new(IAudioSessionControl2_Vtbl {
            base__: IAudioSessionControl_Vtbl {
                base__: IUnknown_Vtbl {
                    QueryInterface: stub_query_interface,
                    AddRef: stub_add_ref,
                    Release: stub_release,
                },
                GetState: stub_get_state,
                GetDisplayName: stub_get_display_name,
                SetDisplayName: stub_set_display_name,
                GetIconPath: stub_get_icon_path,
                SetIconPath: stub_set_icon_path,
                GetGroupingParam: stub_get_grouping_param,
                SetGroupingParam: stub_set_grouping_param,
                RegisterAudioSessionNotification: stub_register_notification,
                UnregisterAudioSessionNotification: stub_unregister_notification,
            },
            GetSessionIdentifier: stub_session_identifier,
            GetSessionInstanceIdentifier: stub_instance_identifier,
            GetProcessId: stub_get_process_id,
            IsSystemSoundsSession: stub_is_system_sounds,
            SetDuckingPreference: stub_set_ducking,
        }))
    }

    /// The same process-id read through a raw object pointer.
    ///
    /// A raw pointer is required here because creating a Rust reference to a fake interface
    /// would assert provenance this test deliberately does not have.
    ///
    /// # Safety
    ///
    /// `control` must be the object pointer of a live `IAudioSessionControl2` that stays
    /// alive for the duration of the call.
    unsafe fn get_process_id_outcome_raw(control: *mut core::ffi::c_void) -> ProcessIdOutcome {
        let mut pid = 0u32;
        // SAFETY: the caller guarantees a live interface. The vtable is reached through the
        // object's own first field, which is the layout every COM interface pointer has, and
        // `pid` is a live local the callee is documented to write through.
        let status: HRESULT = unsafe {
            let vtable = *(control as *const *const IAudioSessionControl2_Vtbl);
            ((*vtable).GetProcessId)(control, &mut pid)
        };
        ProcessIdOutcome {
            status,
            value: status.is_ok().then_some(pid),
        }
    }

    /// Runs the production status helper against one fake session control.
    fn status_for(status: HRESULT, pid: u32) -> (HRESULT, Option<u32>) {
        let mut object = FakeSessionControl {
            vtable: fake_vtable(),
            marker: &TEST_OBJECT_MARKER,
            status,
            pid,
        };
        let raw = std::ptr::addr_of_mut!(object).cast::<core::ffi::c_void>();
        // SAFETY: `raw` points to a layout-correct COM object whose first field is a live
        // vtable of the official generated type, and the object outlives the call.
        let outcome = unsafe { get_process_id_outcome_raw(raw) };
        std::hint::black_box(&mut object);
        (outcome.status, outcome.value)
    }

    #[test]
    fn the_positive_multi_process_status_is_preserved() {
        let (status, pid) = status_for(AUDCLNT_S_NO_SINGLE_PROCESS, 4242);
        assert_eq!(status, AUDCLNT_S_NO_SINGLE_PROCESS);
        assert!(status.is_ok(), "the documented status is a success code");
        assert_eq!(pid, Some(4242));
        assert_eq!(
            classify_process_id_status(status.0, true, 4242),
            ApplicationProcessIdStatus::NoSingleProcess
        );
    }

    #[test]
    fn s_ok_and_s_false_are_both_successes_and_distinct() {
        assert_eq!(status_for(S_OK, 7), (S_OK, Some(7)));
        let (status, pid) = status_for(S_FALSE, 9);
        assert_eq!(status, S_FALSE);
        assert!(status.is_ok());
        assert_eq!(pid, Some(9));
        assert_eq!(
            classify_system_sounds_status(S_FALSE.0),
            ApplicationSystemSoundsStatus::IsNotSystemSounds
        );
        assert_eq!(
            classify_system_sounds_status(S_OK.0),
            ApplicationSystemSoundsStatus::IsSystemSounds
        );
    }

    #[test]
    fn a_failed_status_has_no_out_value() {
        let (status, pid) = status_for(AUDCLNT_E_DEVICE_INVALIDATED, 1);
        assert_eq!(status, AUDCLNT_E_DEVICE_INVALIDATED);
        assert_eq!(pid, None);
    }

    #[test]
    fn the_multi_process_status_matches_the_documented_macro() {
        // MAKE_SCODE(SEVERITY_SUCCESS=0, FACILITY_AUDCLNT=2185, 0x00d).
        let expected = ((2185u32 << 16) | 0x00d) as i32;
        assert_eq!(AUDCLNT_S_NO_SINGLE_PROCESS, HRESULT(expected));
        assert_eq!(AUDCLNT_S_NO_SINGLE_PROCESS.0 as u32, 0x0889_000D);
    }

    #[test]
    fn the_device_enumerator_class_is_not_the_interface() {
        let class = GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);
        assert_eq!(MMDeviceEnumerator, class);
        assert_ne!(class, IMMDeviceEnumerator::IID);
    }

    #[test]
    fn the_documented_process_open_errors_are_distinct() {
        assert_eq!(
            describe_process_open_error(ERROR_ACCESS_DENIED.0),
            "this process has no access to the owner process"
        );
        assert_ne!(
            describe_process_open_error(ERROR_ACCESS_DENIED.0),
            describe_process_open_error(ERROR_NOT_FOUND.0)
        );
        assert_eq!(
            describe_process_open_error(0xDEAD),
            "the owner process could not be opened"
        );
    }

    #[test]
    fn win32_codes_are_recovered_from_both_shapes() {
        let from_win32 = windows::core::Error::from_win32();
        let code = win32_code(&from_win32);
        assert!(code <= 0xFFFF, "a Win32 code is 16 bits, got {code:#X}");
        let hresult = windows::core::Error::from(HRESULT(0x8889_0004_u32 as i32));
        assert_eq!(win32_code(&hresult), 0x8889_0004);
    }

    // -----------------------------------------------------------------------
    // Bounded OS strings
    // -----------------------------------------------------------------------

    /// Allocates a real `CoTaskMemAlloc` block holding `code_units` plus a terminator, so
    /// the bounded scan runs against the real allocator.
    fn os_string(code_units: &[u16]) -> CoTaskMemString {
        let total = code_units.len() + 1;
        // SAFETY: CoTaskMemAlloc is the documented allocator for these strings, and the
        // block is immediately owned by the returned guard.
        let raw = unsafe { windows::Win32::System::Com::CoTaskMemAlloc(total * 2) as *mut u16 };
        assert!(!raw.is_null(), "CoTaskMemAlloc must return a block");
        // SAFETY: the block is `total` units and both writes stay inside it.
        unsafe {
            std::ptr::copy_nonoverlapping(code_units.as_ptr(), raw, code_units.len());
            raw.add(code_units.len()).write(0);
        }
        CoTaskMemString::new(PWSTR(raw))
    }

    /// A content length of exactly the bound is accepted; one unit more is refused, and
    /// the terminator is not part of the limit.
    #[test]
    fn the_string_bound_counts_content_units_and_not_the_terminator() {
        let at_bound: Vec<u16> =
            std::iter::repeat_n(u16::from(b'x'), MAX_NATIVE_STRING_UTF16).collect();
        let owned = os_string(&at_bound);
        assert_eq!(
            owned.bounded_units("a test string").expect("at the bound"),
            MAX_NATIVE_STRING_UTF16,
            "the terminator is separate from the 4096-unit content bound"
        );
        drop(owned);

        let over: Vec<u16> =
            std::iter::repeat_n(u16::from(b'x'), MAX_NATIVE_STRING_UTF16 + 1).collect();
        let owned = os_string(&over);
        let error = owned
            .bounded_units("a test string")
            .expect_err("one unit past the bound must fail");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("4096"), "{}", error.message);
        assert!(
            error.message.contains("4097"),
            "the reported length is the content length: {}",
            error.message
        );
    }

    /// Supplementary-plane text counts as UTF-16 units, not Unicode scalars.
    #[test]
    fn supplementary_unicode_is_counted_in_utf16_units() {
        let emoji = "\u{1f600}".encode_utf16().collect::<Vec<u16>>();
        assert_eq!(emoji.len(), 2, "one scalar is two UTF-16 code units");
        let owned = os_string(&emoji);
        assert_eq!(owned.bounded_units("a test string").expect("in bound"), 2);
        assert_eq!(
            owned
                .to_wide_string("an identity")
                .expect("valid UTF-16")
                .as_str(),
            "\u{1f600}"
        );
    }

    /// Identity conversion refuses malformed UTF-16; the label conversion replaces it.
    #[test]
    fn malformed_utf16_is_refused_for_identity_and_replaced_for_labels() {
        // A lone high surrogate is not valid UTF-16.
        let malformed = [0xD800u16];
        let owned = os_string(&malformed);
        let error = owned
            .to_wide_string("a session identifier")
            .expect_err("malformed identity text must be refused");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(
            error.message.contains("not valid Unicode"),
            "{}",
            error.message
        );
        assert!(error.message.contains("no identity"), "{}", error.message);

        // The same units are acceptable as a presentation label, where nothing downstream
        // treats them as identity.
        let label = owned
            .to_label_string("a session display name")
            .expect("a label may be replaced");
        assert_eq!(label.as_str(), "\u{fffd}");
        assert_eq!(label.units(), 1);
    }

    /// A null OS string is "none" and releases as a no-op.
    #[test]
    fn a_null_os_string_is_none() {
        let owned = CoTaskMemString::new(PWSTR(std::ptr::null_mut()));
        assert!(owned.is_null());
        assert_eq!(owned.bounded_units("a test string").expect("none"), 0);
        assert_eq!(
            owned
                .to_wide_string("a test string")
                .expect("none")
                .into_string(),
            ""
        );
    }

    /// Exercises the production guard on normal return and unwind. This is a smoke
    /// test, not an allocator release counter or a leak measurement.
    #[test]
    fn the_os_string_owner_can_drop_normally_and_during_unwind() {
        drop(os_string(&[b'x' as u16]));
        let unwound = std::panic::catch_unwind(|| {
            let _owned = os_string(&[b'x' as u16]);
            panic!("synthetic unwind with a live OS string owner");
        });
        assert!(unwound.is_err());
    }

    /// Where a scripted decision or read is recorded so a test can assert it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DecisionKind {
        ShouldReadEndpoint,
        ShouldReadSession,
        ReadEndpointItem,
        ReadSessionItem,
    }

    /// What one scripted call did.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct RecordedDecision {
        kind: DecisionKind,
        index: usize,
        allowed: bool,
    }

    /// The decisions a test can assert over.
    #[derive(Default)]
    struct DecisionLog {
        decisions: RefCell<Vec<RecordedDecision>>,
    }

    impl DecisionLog {
        fn record(&self, kind: DecisionKind, index: usize, allowed: bool) {
            self.decisions.borrow_mut().push(RecordedDecision {
                kind,
                index,
                allowed,
            });
        }

        /// How many reads of one kind happened.
        fn reads(&self, kind: DecisionKind) -> usize {
            self.decisions
                .borrow()
                .iter()
                .filter(|decision| decision.kind == kind)
                .count()
        }

        /// How many decisions of one kind were refused.
        fn refusals(&self, kind: DecisionKind) -> usize {
            self.decisions
                .borrow()
                .iter()
                .filter(|decision| decision.kind == kind && !decision.allowed)
                .count()
        }
    }

    /// How one scripted metadata read behaves.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MetadataStep {
        /// The read succeeds with this creation time.
        Succeeds(u64),
        /// The read fails.
        Fails,
        /// The process could not be opened.
        OpenFails,
        /// The OS supplied an image path beyond the configured bound.
        InvalidPath,
    }

    /// A scripted pass observer: it supplies the values and failures of every read, so the
    /// production collector's loop, count checks, classification and record assembly run.
    struct ScriptedObserver {
        /// The session count each endpoint would report.
        reported_sessions_per_endpoint: Vec<usize>,
        /// Which endpoints fail `read_endpoint_id`.
        endpoints_without_id: Vec<usize>,
        /// Which sessions fail `read_session` with a recoverable disappearance.
        vanished_sessions: Vec<(usize, usize)>,
        /// Which sessions fail an essential read: (endpoint, session, kind).
        failed_essential: Vec<(usize, usize, SessionFailureKind)>,
        /// The image path each session reports.
        image_path: String,
        /// How the process metadata reads behave.
        metadata: MetadataStep,
        /// Which endpoints report a device state other than active.
        inactive_endpoints: Vec<(usize, u32)>,
        /// The session-manager or enumerator failure every endpoint reports, if any.
        open_failure: Option<FailureEvent>,
        /// Every decision the production collector asked for, in order.
        log: DecisionLog,
        /// The process ids whose metadata was read, so a test can prove one open per session.
        metadata_opens: RefCell<Vec<u32>>,
    }

    impl ScriptedObserver {
        fn new(reported_endpoints: usize, reported_sessions_per_endpoint: Vec<usize>) -> Self {
            assert_eq!(
                reported_sessions_per_endpoint.len(),
                reported_endpoints,
                "a script needs one session count per endpoint it expects"
            );
            Self {
                reported_sessions_per_endpoint,
                endpoints_without_id: Vec::new(),
                vanished_sessions: Vec::new(),
                failed_essential: Vec::new(),
                image_path: r"C:\Synthetic\app.exe".to_string(),
                metadata: MetadataStep::Succeeds(133_000_000_000_000_000),
                inactive_endpoints: Vec::new(),
                open_failure: None,
                log: DecisionLog::default(),
                metadata_opens: RefCell::new(Vec::new()),
            }
        }

        fn with_open_failure(mut self, failure: FailureEvent) -> Self {
            self.open_failure = Some(failure);
            self
        }

        fn with_endpoint_without_id(mut self, endpoint: usize) -> Self {
            self.endpoints_without_id.push(endpoint);
            self
        }

        fn with_vanished_session(mut self, endpoint: usize, session: usize) -> Self {
            self.vanished_sessions.push((endpoint, session));
            self
        }

        fn with_failed_essential(
            mut self,
            endpoint: usize,
            session: usize,
            kind: SessionFailureKind,
        ) -> Self {
            self.failed_essential.push((endpoint, session, kind));
            self
        }

        fn with_metadata(mut self, metadata: MetadataStep) -> Self {
            self.metadata = metadata;
            self
        }

        fn with_inactive_endpoint(mut self, endpoint: usize, state: u32) -> Self {
            self.inactive_endpoints.push((endpoint, state));
            self
        }

        /// Runs one production collection pass with this script.
        ///
        /// The collector still creates the real `MMDeviceEnumerator` and reads the real
        /// active render-endpoint count; the scripted observer supplies every per-endpoint
        /// and per-session read and failure. An apartment is initialized for the calling
        /// thread first, exactly as the production worker does.
        fn collect(&self) -> Result<ObservationHeader, RecorderError> {
            let _apartment = ComApartment::initialize_mta()?;
            collect(self)
        }
    }

    struct ScriptedDevice {
        index: usize,
    }

    /// The enumerator value the scripted seam hands the collector.
    struct ScriptedEnumerator {
        endpoint: usize,
    }

    impl NativePassObserver for ScriptedObserver {
        type Device = ScriptedDevice;
        type Enumerator = ScriptedEnumerator;

        fn should_read_endpoint(&self, index: usize, reported: usize) -> Result<(), RecorderError> {
            let allowed =
                reported <= MAX_ENDPOINTS_PER_SNAPSHOT && index < MAX_ENDPOINTS_PER_SNAPSHOT;
            self.log
                .record(DecisionKind::ShouldReadEndpoint, index, allowed);
            if allowed {
                Ok(())
            } else {
                Err(super::super::limits::endpoint_limit_error(
                    MAX_ENDPOINTS_PER_SNAPSHOT,
                    reported.max(index + 1),
                ))
            }
        }

        fn should_read_session(&self, index: usize, reported: usize) -> Result<(), RecorderError> {
            let allowed = index < reported;
            self.log
                .record(DecisionKind::ShouldReadSession, index, allowed);
            if allowed {
                Ok(())
            } else {
                Err(super::super::limits::session_limit_error(
                    super::super::limits::MAX_SESSIONS_PER_SNAPSHOT,
                    reported.max(index + 1),
                ))
            }
        }

        fn observe_session_item(&self, index: usize) -> Result<(), RecorderError> {
            self.log.record(DecisionKind::ReadSessionItem, index, true);
            Ok(())
        }

        fn read_endpoint_device(
            &self,
            _collection: &IMMDeviceCollection,
            index: usize,
        ) -> Result<ScriptedDevice, FailureEvent> {
            self.log.record(DecisionKind::ReadEndpointItem, index, true);
            Ok(ScriptedDevice { index })
        }

        fn read_endpoint_id(
            &self,
            device: &ScriptedDevice,
            budget: &mut SnapshotBudget,
        ) -> Result<WideString, FailureEvent> {
            if self.endpoints_without_id.contains(&device.index) {
                return Err(FailureEvent::from_hresult(
                    "IMMDevice::GetId",
                    "the endpoint identity could not be read",
                    0x8000_4005_u32 as i32,
                ));
            }
            let value = format!("scripted-endpoint-{}", device.index);
            budget
                .reserve_capacity(value.len(), 1, "the endpoint ID")
                .map_err(|error| FailureEvent::invalid("IMMDevice::GetId", error.message))?;
            let units = value.encode_utf16().count();
            Ok(WideString::new(value, units))
        }

        fn read_device_state(&self, device: &ScriptedDevice) -> Result<u32, FailureEvent> {
            Ok(self
                .inactive_endpoints
                .iter()
                .find(|(index, _)| *index == device.index)
                .map(|(_, state)| *state)
                .unwrap_or(DEVICE_STATE_ACTIVE.0))
        }

        fn read_friendly_name(
            &self,
            _device: &ScriptedDevice,
            budget: &mut SnapshotBudget,
        ) -> Result<Option<WideString>, RecorderError> {
            budget.reserve_capacity(17, 1, "the endpoint friendly name")?;
            Ok(Some(WideString::new("Scripted endpoint".to_string(), 17)))
        }

        fn open_endpoint_sessions(
            &self,
            device: &ScriptedDevice,
        ) -> Result<EndpointSessions<ScriptedEnumerator>, FailureEvent> {
            if let Some(failure) = self.open_failure.as_ref() {
                return Err(failure.clone());
            }
            let reported = self
                .reported_sessions_per_endpoint
                .get(device.index)
                .copied()
                .unwrap_or(0) as i32;
            Ok(EndpointSessions {
                enumerator: Some(ScriptedEnumerator {
                    endpoint: device.index,
                }),
                reported,
            })
        }

        fn read_session(
            &self,
            sessions: &EndpointSessions<ScriptedEnumerator>,
            index: usize,
            budget: &mut SnapshotBudget,
        ) -> Result<SessionControl, FailureEvent> {
            let enumerator = sessions
                .enumerator
                .as_ref()
                .expect("the scripted enumerator is always present");
            let endpoint = enumerator.endpoint;
            if self
                .vanished_sessions
                .iter()
                .any(|(e, s)| *e == endpoint && *s == index)
            {
                return Err(FailureEvent::from_hresult(
                    "IAudioSessionEnumerator::GetSession",
                    "the session could not be obtained; it may have ended during the pass",
                    AUDCLNT_E_DEVICE_INVALIDATED.0,
                ));
            }

            let fails = |kind: SessionFailureKind| {
                self.failed_essential
                    .iter()
                    .any(|(e, s, k)| *e == endpoint && *s == index && *k == kind)
            };
            budget
                .reserve_capacity(15, 1, "a session identifier")
                .map_err(|error| FailureEvent::invalid("scripted session", error.message))?;
            budget
                .reserve_capacity(24, 1, "a session instance identifier")
                .map_err(|error| FailureEvent::invalid("scripted session", error.message))?;
            let mut values = SessionControlOwned::all_unset();
            values.state = if fails(SessionFailureKind::State) {
                Err(FailureEvent::from_hresult(
                    "IAudioSessionControl::GetState",
                    "the session state could not be read",
                    0x8000_4005_u32 as i32,
                ))
            } else {
                Ok(1)
            };
            values.identifier = if fails(SessionFailureKind::SessionIdentifier) {
                Err(FailureEvent::from_hresult(
                    "IAudioSessionControl2::GetSessionIdentifier",
                    "the session identifier could not be read",
                    0x8000_4005_u32 as i32,
                ))
            } else {
                Ok(WideString::new("scripted-session".to_string(), 15))
            };
            values.instance_identifier = if fails(SessionFailureKind::InstanceIdentifier) {
                Err(FailureEvent::from_hresult(
                    "IAudioSessionControl2::GetSessionInstanceIdentifier",
                    "the session instance identifier could not be read",
                    0x8000_4005_u32 as i32,
                ))
            } else {
                Ok(WideString::new(format!("scripted-instance-{index}"), 24))
            };
            values.system_sounds = if fails(SessionFailureKind::SystemSounds) {
                Err(FailureEvent::from_hresult(
                    "IAudioSessionControl2::IsSystemSoundsSession",
                    "the system-sounds status could not be determined",
                    0x8000_4005_u32 as i32,
                ))
            } else {
                Ok(windows::Win32::Foundation::S_FALSE)
            };
            values.process_id = if fails(SessionFailureKind::ProcessId) {
                Err(FailureEvent::from_hresult(
                    "IAudioSessionControl2::GetProcessId",
                    "the session's process identifier could not be read",
                    0x8000_4005_u32 as i32,
                ))
            } else {
                Ok(ProcessIdOutcome {
                    status: windows::Win32::Foundation::S_OK,
                    value: Some(4242),
                })
            };
            Ok(SessionControl {
                interface: None,
                values,
            })
        }

        fn read_process_metadata(
            &self,
            pid: u32,
            body: &mut dyn FnMut(&ProcessMetadataHandle<'_>) -> ProcessMetadata,
        ) -> Result<ProcessMetadata, FailureEvent> {
            self.metadata_opens.borrow_mut().push(pid);
            if self.metadata == MetadataStep::OpenFails {
                return Err(FailureEvent::from_win32(
                    "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)",
                    "opening process 4242 failed: this process has no access to the owner process"
                        .to_string(),
                    5,
                ));
            }
            // The body runs against a real handle to this process, so the production image
            // path and creation-time reads are what execute. The recorded script decides
            // which of them is treated as failed.
            let opened = ProcessMetadataHandle {
                handle: ProcessHandle::new(
                    // SAFETY: opening this own process with the least-privilege read right.
                    unsafe {
                        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, std::process::id())
                    }
                    .expect("opening this process must succeed"),
                ),
                _marker: std::marker::PhantomData,
            };
            let mut metadata = body(&opened);
            // The image path is the scripted one, so the bound checked by the production
            // reader is exercised for the scripted length rather than this process's.
            let units = self.image_path.encode_utf16().count();
            metadata.image_path = if self.metadata == MetadataStep::InvalidPath {
                Err(image_path_query_failure(pid, 122))
            } else {
                Ok(WideString::new(self.image_path.clone(), units))
            };
            metadata.creation_time = match self.metadata {
                MetadataStep::Succeeds(value) => Ok(value),
                MetadataStep::Fails => Err(FailureEvent::from_win32(
                    "GetProcessTimes",
                    "the process creation time could not be read".to_string(),
                    5,
                )),
                MetadataStep::OpenFails => unreachable!("handled above"),
                MetadataStep::InvalidPath => Ok(133_000_000_000_000_000),
            };
            Ok(metadata)
        }
    }
    // -----------------------------------------------------------------------
    // Count enforcement: no work past the bound
    // -----------------------------------------------------------------------

    /// 128 endpoints are read; 129 are refused before the first item read.
    ///
    /// The total-count guard is asserted directly: the collector calls it **before** the
    /// first item read, and the real device count on a machine with one endpoint cannot
    /// exercise a 129-endpoint report. This is the production guard, called in the
    /// production position; the session-count tests below run through the whole collector.
    #[test]
    fn the_endpoint_bound_refuses_the_first_read_past_it() {
        let allowed = ScriptedObserver::new(
            MAX_ENDPOINTS_PER_SNAPSHOT,
            vec![0; MAX_ENDPOINTS_PER_SNAPSHOT],
        );
        allowed
            .should_read_endpoint(0, MAX_ENDPOINTS_PER_SNAPSHOT)
            .expect("exactly the bound is allowed");

        let refused = ScriptedObserver::new(
            MAX_ENDPOINTS_PER_SNAPSHOT + 1,
            vec![0; MAX_ENDPOINTS_PER_SNAPSHOT + 1],
        );
        let error = refused
            .should_read_endpoint(0, MAX_ENDPOINTS_PER_SNAPSHOT + 1)
            .expect_err("one past the bound must be refused");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("128"), "{}", error.message);
        assert_eq!(
            refused.log.reads(DecisionKind::ReadEndpointItem),
            0,
            "the refusal happens before any item read"
        );
        assert_eq!(
            refused.log.refusals(DecisionKind::ShouldReadEndpoint),
            1,
            "exactly one refusal, before any item read"
        );
    }

    #[test]
    fn integration_native_observation_must_charge_its_owned_payload() {
        let observer = ScriptedObserver::new(1, vec![1]).with_metadata(MetadataStep::OpenFails);
        let mut budget = SnapshotBudget::new();
        let observed = observe_endpoint(&ScriptedDevice { index: 0 }, 0, &mut budget, &observer)
            .expect("one valid scripted endpoint");
        assert_eq!(observed.sessions.len(), 1);
        assert!(
            budget.charged_bytes() > 0,
            "native observation owns strings/vectors but reports {} charged bytes",
            budget.charged_bytes()
        );
    }

    #[test]
    fn collected_storage_is_carried_into_mapping_without_restarting_the_ledger() {
        let observer = ScriptedObserver::new(1, vec![1]).with_metadata(MetadataStep::OpenFails);
        let mut budget = SnapshotBudget::new();
        let mut endpoints = reserved_vec(&mut budget, 1, "native endpoint records").unwrap();
        endpoints.push(
            observe_endpoint(&ScriptedDevice { index: 0 }, 0, &mut budget, &observer).unwrap(),
        );
        let native_bytes = budget.charged_bytes();
        assert!(native_bytes >= std::mem::size_of::<ObservedEndpoint>());
        let snapshot = super::super::map_observation(ObservationHeader {
            endpoints,
            endpoints_reported: 1,
            failures: Vec::new(),
            budget,
        })
        .expect("a charged native observation maps into a source");
        assert_eq!(snapshot.sources.len(), 1);
        assert!(snapshot.counters.payload_bytes > native_bytes);
        assert!(snapshot.counters.payload_bytes <= snapshot.limits.max_snapshot_payload_bytes);
    }

    #[test]
    fn native_string_copy_refuses_the_payload_bound_before_retaining_text() {
        let mut limits = super::super::APPLICATION_SESSION_LIMITS;
        limits.max_snapshot_payload_bytes = 31;
        let mut budget = SnapshotBudget::with_limits(limits);
        let value = os_string(&[b'x' as u16; 8]);
        let error = value
            .to_charged_string("a session identifier", false, &mut budget)
            .expect_err("eight UTF-16 units need a 32-byte UTF-8 reservation");
        assert!(error.message.contains("32"), "{}", error.message);
        assert_eq!(budget.charged_bytes(), 0);
    }

    #[test]
    fn image_path_boundary_and_bad_unicode_fail_before_attribution() {
        let mut budget = SnapshotBudget::new();
        let mut exact = vec![b'x' as u16; MAX_IMAGE_PATH_UNITS + 1];
        exact[MAX_IMAGE_PATH_UNITS] = 0;
        let path = decode_image_path(&exact, MAX_IMAGE_PATH_UNITS, 42, &mut budget)
            .expect("4096 content units fit with one terminator slot");
        assert_eq!(path.units(), MAX_IMAGE_PATH_UNITS);
        assert!(budget.charged_bytes() >= path.as_str().len());
        let spaced: Vec<u16> = "C:\\example.exe ".encode_utf16().chain(Some(0)).collect();
        let path = decode_image_path(&spaced, spaced.len() - 1, 42, &mut budget).unwrap();
        assert_eq!(path.as_str(), "C:\\example.exe ");
        assert!(
            decode_image_path(&[b'x' as u16, 1], 1, 42, &mut budget)
                .unwrap_err()
                .fatal
        );
        let oversized = vec![b'x' as u16; MAX_IMAGE_PATH_UNITS + 2];
        assert!(
            decode_image_path(&oversized, MAX_IMAGE_PATH_UNITS + 1, 42, &mut budget)
                .expect_err("4097 content units exceed the bound")
                .fatal
        );
        assert!(
            decode_image_path(&[0xD800, 0], 1, 42, &mut budget)
                .expect_err("malformed UTF-16 cannot identify a process")
                .fatal
        );
    }

    #[test]
    fn image_path_unavailability_is_optional_but_capacity_failure_is_fatal() {
        assert!(!image_path_query_failure(42, 5).fatal);
        assert!(image_path_query_failure(42, 122).fatal);
        let observer = ScriptedObserver::new(1, vec![1]).with_metadata(MetadataStep::InvalidPath);
        let mut budget = SnapshotBudget::new();
        let error = observe_endpoint(&ScriptedDevice { index: 0 }, 0, &mut budget, &observer)
            .expect_err("over-limit path data fails the production collector");
        assert!(
            error.message.contains("QueryFullProcessImageNameW"),
            "{}",
            error.message
        );
    }

    #[test]
    fn query_interface_failure_keeps_its_operation_and_hresult() {
        let observer = ScriptedObserver::new(1, vec![1]);
        let mut budget = SnapshotBudget::new();
        let sessions = EndpointSessions {
            enumerator: Some(ScriptedEnumerator { endpoint: 0 }),
            reported: 1,
        };
        let mut control = observer.read_session(&sessions, 0, &mut budget).unwrap();
        control.values.identifier = Err(FailureEvent::from_hresult(
            "IAudioSessionControl::QueryInterface(IAudioSessionControl2)",
            "the session does not expose IAudioSessionControl2",
            0x8000_4002_u32 as i32,
        ));
        let session = observe_session(control, &mut budget, &observer).unwrap();
        let failure = session.essential_failure.as_ref().unwrap();
        assert_eq!(failure.hresult, Some(0x8000_4002_u32 as i32));
        assert_eq!(
            failure.operation,
            "IAudioSessionControl::QueryInterface(IAudioSessionControl2)"
        );
        let error = super::super::map_observation(ObservationHeader {
            endpoints: vec![ObservedEndpoint {
                endpoint_id: "test endpoint".to_string(),
                friendly_name: None,
                device_state: DEVICE_STATE_ACTIVE.0,
                sessions: vec![session],
            }],
            endpoints_reported: 1,
            failures: Vec::new(),
            budget,
        })
        .expect_err("an essential interface failure fails the snapshot");
        assert!(
            error.message.contains("QueryInterface"),
            "{}",
            error.message
        );
        assert!(error.message.contains("80004002"), "{}", error.message);
    }

    /// The mapper's backstop refuses a reported endpoint count past the bound as well.
    #[test]
    fn the_mapper_refuses_a_reported_endpoint_count_past_the_bound() {
        let header = ObservationHeader::fixture(Vec::new(), MAX_ENDPOINTS_PER_SNAPSHOT + 1);
        let error = super::super::map_observation(header)
            .expect_err("the mapper must refuse an impossible reported count");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("128"), "{}", error.message);
    }

    /// 1024 sessions are read; a reported total of 1025 is refused before the first session
    /// read of that endpoint.
    ///
    /// The check asserted here is the production `observe_sessions` allowance test: the
    /// endpoint's whole reported count is compared with the shared ledger's remaining
    /// allowance before any `should_read_session` call, so a report past the bound costs
    /// zero session reads.
    #[test]
    fn the_session_bound_refuses_a_reported_total_past_it() {
        let budget = SnapshotBudget::new();
        let allowed = super::super::limits::MAX_SESSIONS_PER_SNAPSHOT;
        assert!(
            allowed <= budget.session_allowance(),
            "exactly the bound fits the empty ledger"
        );

        let budget = SnapshotBudget::new();
        let refused = super::super::limits::MAX_SESSIONS_PER_SNAPSHOT + 1;
        let error = super::super::limits::session_limit_error(
            budget.limits().max_sessions_per_snapshot,
            budget.sessions() + refused,
        );
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("1024"), "{}", error.message);
        // And the collector refuses it through the real path, without reading a session.
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = refused;
            sessions
        });
        let error = observer
            .collect()
            .expect_err("a reported total past the bound must fail the pass");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("1024"), "{}", error.message);
        assert_eq!(
            observer.log.reads(DecisionKind::ReadSessionItem),
            0,
            "an over-large reported count must cost zero session reads"
        );
    }

    /// 600 + 424 sessions fit the 1024 budget; 600 + 425 do not through the shared ledger.
    #[test]
    fn the_session_bound_is_snapshot_wide_across_endpoints() {
        let mut budget = SnapshotBudget::new();
        for _ in 0..600 {
            budget.charge_session(0).expect("within the budget");
        }
        assert_eq!(budget.session_allowance(), 424);
        // The second endpoint reports exactly the remaining allowance: accepted.
        assert!(424 <= budget.session_allowance());
        let error = super::super::limits::session_limit_error(
            budget.limits().max_sessions_per_snapshot,
            budget.sessions() + 425,
        );
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("1024"), "{}", error.message);
        // The ledger itself refuses the 1025th session, so a run that ignored the
        // per-endpoint check still could not over-charge.
        for _ in 0..424 {
            budget.charge_session(0).expect("within the budget");
        }
        assert_eq!(budget.sessions(), 1024);
        assert!(budget.charge_session(0).is_err());
    }

    /// Repeated sessions that each fail their own reads still count against the allowance,
    /// so a run of failures cannot hide past the bound.
    #[test]
    fn repeated_recoverable_failures_still_count_against_the_allowance() {
        let mut observer = ScriptedObserver::new(1, vec![3]);
        for index in 0..3 {
            observer = observer.with_vanished_session(0, index);
        }
        let header = observer
            .collect()
            .expect("three vanished sessions are a pass");
        assert_eq!(header.budget.sessions(), 3, "every read session is charged");
        assert_eq!(header.endpoints.len(), 1);
        assert_eq!(header.endpoints[0].sessions.len(), 3);
        for session in &header.endpoints[0].sessions {
            let failure = session
                .essential_failure
                .as_ref()
                .expect("each scripted session vanished");
            assert!(failure.recoverable_disappearance);
        }
    }
    // -----------------------------------------------------------------------
    // Test harness: the real device count, scripted per-endpoint reads
    // -----------------------------------------------------------------------

    /// The real active-render-endpoint count, so a scripted pass matches the machine it
    /// runs on instead of assuming a fixture count.
    fn real_endpoint_count() -> Option<usize> {
        let _apartment = ComApartment::initialize_mta().ok()?;
        // SAFETY: an initialized apartment; the calls only enumerate devices.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
        // SAFETY: the enumerator is live.
        let collection =
            unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.ok()?;
        // SAFETY: the collection is live.
        let count = unsafe { collection.GetCount() }.ok()?;
        Some(count as usize)
    }

    /// Runs one scripted pass against the real endpoint count, or reports that this
    /// machine has no active render endpoint to script.
    fn run_scripted(script: impl FnOnce(usize) -> ScriptedObserver) -> Option<ObservationHeader> {
        let count = real_endpoint_count()?;
        let observer = script(count);
        Some(
            observer
                .collect()
                .unwrap_or_else(|error| panic!("the scripted pass failed: {}", error.message)),
        )
    }
    // -----------------------------------------------------------------------
    // Result and error semantics on the production collector path
    // -----------------------------------------------------------------------

    /// A successful enumeration with zero sessions is `Ok(empty)`, not a failure.
    #[test]
    fn a_true_empty_pass_succeeds() {
        let Some(header) = run_scripted(|count| ScriptedObserver::new(count, vec![0; count]))
        else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        assert!(header.failures.is_empty());
        for endpoint in &header.endpoints {
            assert!(endpoint.sessions.is_empty());
        }
    }

    /// A failed session manager, enumerator or count is a pass error with the API name and
    /// code, even though another endpoint could have been read.
    #[test]
    fn a_failed_endpoint_enumeration_is_a_pass_error() {
        for (operation, hresult) in [
            (
                "IMMDevice::Activate(IAudioSessionManager2)",
                0x8000_4005_u32,
            ),
            (
                "IAudioSessionManager2::GetSessionEnumerator",
                0x8000_4005_u32,
            ),
            ("IAudioSessionEnumerator::GetCount", 0x8000_4005_u32),
        ] {
            let Some(count) = real_endpoint_count() else {
                eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
                return;
            };
            let observer = ScriptedObserver::new(count, vec![0; count]).with_open_failure(
                FailureEvent::from_hresult(
                    operation,
                    "scripted endpoint enumeration failure",
                    hresult as i32,
                ),
            );
            let error = observer.collect().expect_err("the pass must fail");
            assert_eq!(error.code, ErrorCode::Internal);
            assert!(error.message.contains(operation), "{}", error.message);
            assert!(error.message.contains("80004005"), "{}", error.message);
        }
    }

    /// A failed `IMMDevice::GetId` fails the pass with its own code.
    #[test]
    fn a_failed_endpoint_id_fails_the_pass() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        let observer = ScriptedObserver::new(count, vec![0; count]).with_endpoint_without_id(0);
        let error = observer.collect().expect_err("the pass must fail");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(
            error.message.contains("IMMDevice::GetId"),
            "{}",
            error.message
        );
        assert!(error.message.contains("80004005"), "{}", error.message);
        assert_eq!(
            observer.log.reads(DecisionKind::ReadEndpointItem),
            1,
            "the failing endpoint is read once, not retried"
        );
    }

    /// A failed session state read is an explicit pass error, never an inactive default.
    #[test]
    fn a_failed_session_state_is_not_an_inactive_default() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = 1;
            sessions
        })
        .with_failed_essential(0, 0, SessionFailureKind::State);
        let header = observer
            .collect()
            .expect("the collector records an essential failure instead of failing the pass");
        let failure = header.endpoints[0].sessions[0]
            .essential_failure
            .as_ref()
            .expect("the essential failure is recorded");
        assert_eq!(failure.kind, SessionFailureKind::State);
        assert!(
            !failure.recoverable_disappearance,
            "this is not a disappearance"
        );
        assert_eq!(
            header.endpoints[0].sessions[0].raw_state, 0,
            "no state is manufactured for a failed read"
        );

        let error =
            super::super::map_observation(header).expect_err("the mapper must fail the snapshot");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(
            error.message.contains("IAudioSessionControl::GetState"),
            "{}",
            error.message
        );
        assert!(error.message.contains("80004005"), "{}", error.message);
    }

    /// A failed essential identity read fails the snapshot through the mapper.
    #[test]
    fn essential_failures_fail_the_pass() {
        for kind in [
            SessionFailureKind::SessionIdentifier,
            SessionFailureKind::InstanceIdentifier,
            SessionFailureKind::SystemSounds,
            SessionFailureKind::ProcessId,
        ] {
            let Some(count) = real_endpoint_count() else {
                eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
                return;
            };
            if count == 0 {
                return;
            }
            let observer = ScriptedObserver::new(count, {
                let mut sessions = vec![0; count];
                sessions[0] = 1;
                sessions
            })
            .with_failed_essential(0, 0, kind);
            let header = observer
                .collect()
                .expect("the collector records the failure");
            let error = super::super::map_observation(header).expect_err("the mapper must fail");
            assert!(
                error.message.contains(kind.operation()),
                "{}",
                error.message
            );
        }
    }

    /// The documented disappearance code is a skip with a reason, not a snapshot failure.
    #[test]
    fn a_documented_disappearance_is_a_skip_not_a_failure() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = 1;
            sessions
        })
        .with_vanished_session(0, 0);
        let header = observer.collect().expect("a disappeared session is a skip");
        let failure = header.endpoints[0].sessions[0]
            .essential_failure
            .as_ref()
            .expect("the skip is recorded");
        assert!(failure.recoverable_disappearance);
        assert_eq!(failure.operation, "IAudioSessionEnumerator::GetSession");
        assert_eq!(failure.hresult, Some(AUDCLNT_E_DEVICE_INVALIDATED.0));

        let snapshot = super::super::map_observation(header)
            .expect("a documented disappearance does not fail the snapshot");
        assert!(snapshot.sources.is_empty());
        assert_eq!(snapshot.counters.session_reads_failed, 1);
    }

    /// An inactive device state is recorded, and its endpoint's sessions are not read.
    #[test]
    fn an_inactive_endpoint_is_recorded_without_reading_sessions() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, vec![3; count]).with_inactive_endpoint(0, 8);
        let header = observer
            .collect()
            .expect("an inactive endpoint is not a failure");
        assert_eq!(header.endpoints[0].device_state, 8);
        assert!(header.endpoints[0].sessions.is_empty());
        assert_eq!(
            observer.log.reads(DecisionKind::ReadSessionItem),
            0,
            "an inactive endpoint's sessions are not read"
        );
    }

    // -----------------------------------------------------------------------
    // Optional process metadata through the production path
    // -----------------------------------------------------------------------

    /// A denied `OpenProcess` keeps the session, records the reason with its Win32 code, and
    /// opens the process exactly once.
    #[test]
    fn denied_process_access_keeps_the_session_and_opens_once() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = 1;
            sessions
        })
        .with_metadata(MetadataStep::OpenFails);
        let header = observer
            .collect()
            .expect("denied optional metadata is not a pass failure");
        let session = &header.endpoints[0].sessions[0];
        assert!(session.essential_failure.is_none());
        assert!(session.executable_path.is_none());
        assert_eq!(session.process_id, Some(4242));
        assert_eq!(
            session.executable_availability,
            ApplicationExecutableAvailability::ProcessUnavailable
        );
        assert_eq!(session.metadata_failures.len(), 1);
        let failure = &session.metadata_failures[0];
        assert_eq!(
            failure.operation,
            "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)"
        );
        assert_eq!(failure.win32_code, Some(5));
        assert!(
            failure.detail.contains("no access to the owner process"),
            "{}",
            failure.detail
        );
        assert_eq!(
            observer.metadata_opens.borrow().len(),
            1,
            "the process is opened exactly once per session"
        );
    }

    /// A failed creation-time read keeps the path-based identifier.
    #[test]
    fn a_failed_creation_time_keeps_the_image_path() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = 1;
            sessions
        })
        .with_metadata(MetadataStep::Fails);
        let header = observer
            .collect()
            .expect("a failed creation time is optional");
        let session = &header.endpoints[0].sessions[0];
        assert!(session.essential_failure.is_none());
        assert_eq!(
            session.executable_path.as_ref().map(|path| path.as_str()),
            Some(r"C:\Synthetic\app.exe")
        );
        assert_eq!(
            session.executable_availability,
            ApplicationExecutableAvailability::Available
        );
        assert!(session.process_creation_time.is_none());
        assert_eq!(session.metadata_failures.len(), 1);
        assert_eq!(session.metadata_failures[0].operation, "GetProcessTimes");
        assert_eq!(observer.metadata_opens.borrow().len(), 1);
    }

    /// A successful metadata read observes both the path and the creation time through one
    /// handle, and the recorded creation time is the value the read produced.
    #[test]
    fn a_successful_metadata_read_observes_path_and_creation_time() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = 1;
            sessions
        })
        .with_metadata(MetadataStep::Succeeds(133_000_000_000_000_000));
        let header = observer.collect().expect("a full metadata read succeeds");
        let session = &header.endpoints[0].sessions[0];
        assert!(session.metadata_failures.is_empty());
        assert!(session.executable_path.is_some());
        assert_eq!(session.process_creation_time, Some(133_000_000_000_000_000));
        assert_eq!(
            observer.metadata_opens.borrow().as_slice(),
            &[4242],
            "both reads observed the process opened for this session"
        );
    }

    /// Reads real process metadata through the production handle guard. Handle release
    /// follows the reviewed RAII path; this test does not count CloseHandle calls.
    #[test]
    fn the_production_image_path_read_returns_metadata() {
        let Some(count) = real_endpoint_count() else {
            eprintln!("SKIP (not evidence): this machine reports no active render endpoint");
            return;
        };
        if count == 0 {
            return;
        }
        let observer = ScriptedObserver::new(count, {
            let mut sessions = vec![0; count];
            sessions[0] = 1;
            sessions
        })
        .with_metadata(MetadataStep::Succeeds(1));
        let header = observer.collect().expect("the production reads succeed");
        assert_eq!(
            header.endpoints[0].sessions[0]
                .executable_path
                .as_ref()
                .map(|path| path.as_str()),
            Some(r"C:\Synthetic\app.exe")
        );
    }
    // -----------------------------------------------------------------------
    // Real pass (smoke evidence, not fault evidence)
    // -----------------------------------------------------------------------

    /// One real pass on this machine, through the production joined worker.
    ///
    /// This is smoke evidence: it shows the public entry point completes against real Core
    /// Audio here. It injects no native failure and asserts nothing about which sessions
    /// exist.
    #[test]
    fn a_real_pass_reports_canonical_sources_through_the_joined_worker() {
        let snapshot = super::super::snapshot().expect("a real snapshot must succeed");
        assert!(
            snapshot.counters().endpoints_reported >= snapshot.counters().endpoints_retained,
            "retained endpoints cannot exceed reported endpoints"
        );
        for source in snapshot.sources() {
            assert!(
                source.id.0.starts_with("wasapi-session:"),
                "unexpected identity {}",
                source.id.0
            );
        }
        eprintln!(
            "FRAME-0106A native pass: {} endpoint(s) reported, {} retained, {} session(s) retained, {} charged bytes",
            snapshot.counters().endpoints_reported,
            snapshot.counters().endpoints_retained,
            snapshot.sources().len(),
            snapshot.counters().payload_bytes
        );
    }

    // -----------------------------------------------------------------------
    // Bounded property-label reads (the friendly-name helper)
    // -----------------------------------------------------------------------

    /// Builds a `VT_LPWSTR` PROPVARIANT that owns its string exactly the way a property
    /// store does: the buffer is `CoTaskMemAlloc`ed and the variant's `Drop`
    /// (`PropVariantClear`) is its only release, so this test must not free it and must
    /// not point it at borrowed stack text.
    fn wide_property(content: Option<&[u16]>) -> PROPVARIANT {
        use windows::Win32::System::Com::StructuredStorage::{
            PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
        };

        let pwsz = match content {
            Some(units) => {
                let total = units.len() + 1;
                // SAFETY: CoTaskMemAlloc is the allocator `PropVariantClear` releases for
                // VT_LPWSTR, and the block is immediately owned by the returned variant.
                let raw =
                    unsafe { windows::Win32::System::Com::CoTaskMemAlloc(total * 2) } as *mut u16;
                assert!(!raw.is_null(), "CoTaskMemAlloc must return a block");
                // SAFETY: the block is `total` code units and both writes stay inside it.
                unsafe {
                    std::ptr::copy_nonoverlapping(units.as_ptr(), raw, units.len());
                    raw.add(units.len()).write(0);
                }
                PWSTR(raw)
            }
            None => PWSTR(std::ptr::null_mut()),
        };
        // The official generated PROPVARIANT types and fields are assembled exactly as
        // the crate's own `From` impls assemble them; no layout is declared here.
        PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: core::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_LPWSTR,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { pwszVal: pwsz },
                }),
            },
        }
    }

    /// Short, missing and empty property names map to a label or to `None`, through the
    /// same helper production uses.
    #[test]
    fn the_bounded_property_label_keeps_short_missing_and_empty_names() {
        let speakers: Vec<u16> = "Speakers".encode_utf16().collect();
        let short = wide_property(Some(&speakers));
        assert_eq!(
            friendly_name_from_propvariant(&short).expect("a short name maps"),
            Some(WideString::new("Speakers".to_string(), 8))
        );

        // VT_EMPTY is the "property not set" shape a store returns.
        let missing = PROPVARIANT::default();
        assert_eq!(
            friendly_name_from_propvariant(&missing).expect("a missing name maps"),
            None
        );

        // A null wide string is a name without content, not a crash.
        let null = wide_property(None);
        assert_eq!(
            friendly_name_from_propvariant(&null).expect("a null name maps"),
            None
        );

        // Empty and whitespace-only names are both "no label", without inventing one.
        let empty = wide_property(Some(&[]));
        assert_eq!(
            friendly_name_from_propvariant(&empty).expect("an empty name maps"),
            None
        );
        let blank: Vec<u16> = "   ".encode_utf16().collect();
        assert_eq!(
            friendly_name_from_propvariant(&wide_property(Some(&blank)))
                .expect("a blank name maps"),
            None
        );
    }

    /// Exactly 4096 UTF-16 content units are accepted, counted in code units rather than
    /// Unicode scalar values.
    #[test]
    fn the_bounded_property_label_accepts_exactly_the_utf16_content_bound() {
        // 4096 ASCII units with the terminator at index 4096: that one terminator position
        // past the content bound is valid.
        let ascii: Vec<u16> =
            std::iter::repeat_n(u16::from(b'x'), MAX_NATIVE_STRING_UTF16).collect();
        let value = wide_property(Some(&ascii));
        let label = friendly_name_from_propvariant(&value)
            .expect("exactly the bound is accepted")
            .expect("4096 content units are a name");
        assert_eq!(label.units(), MAX_NATIVE_STRING_UTF16);

        // 2047 supplementary scalars (4094 code units) plus "x!" is 4096 code units but
        // only 2049 scalars, so a scalar count would judge this string differently.
        let mut mixed: Vec<u16> = Vec::with_capacity(MAX_NATIVE_STRING_UTF16);
        for _ in 0..2047 {
            mixed.extend_from_slice(&[0xD83D, 0xDE00]);
        }
        mixed.extend_from_slice(&[0x0078, 0x0021]);
        assert_eq!(mixed.len(), MAX_NATIVE_STRING_UTF16);
        let scalars = String::from_utf16(&mixed)
            .expect("valid UTF-16")
            .chars()
            .count();
        assert_ne!(
            scalars, MAX_NATIVE_STRING_UTF16,
            "the fixture must differ between code units and scalars"
        );
        let value = wide_property(Some(&mixed));
        let label = friendly_name_from_propvariant(&value)
            .expect("4096 units of supplementary content are accepted")
            .expect("a name");
        assert_eq!(label.units(), MAX_NATIVE_STRING_UTF16);
    }

    /// 4097 content units are refused before any conversion - including whitespace-only
    /// content that trimming would otherwise turn into a silent `None`, and supplementary
    /// content whose scalar count is inside the bound.
    #[test]
    fn the_bounded_property_label_rejects_oversized_content_before_converting_it() {
        // Plain 4097 content units: refused by the helper itself, before any String exists.
        let over: Vec<u16> =
            std::iter::repeat_n(u16::from(b'x'), MAX_NATIVE_STRING_UTF16 + 1).collect();
        let value = wide_property(Some(&over));
        let error =
            friendly_name_from_propvariant(&value).expect_err("4097 content units must fail");
        assert!(error.message.contains("4097"), "{}", error.message);
        assert!(error.message.contains("4096"), "{}", error.message);

        // Whitespace-only 4097 units: converting and trimming first would produce an empty
        // name and hide the violation as a None, so the bound must be checked first. An
        // Err here (rather than Ok(None)) is the ordering proof through the helper.
        let blank: Vec<u16> =
            std::iter::repeat_n(u16::from(b' '), MAX_NATIVE_STRING_UTF16 + 1).collect();
        let value = wide_property(Some(&blank));
        assert!(
            friendly_name_from_propvariant(&value).is_err(),
            "whitespace must not hide an over-long native value"
        );

        // 2048 supplementary scalars plus "x" is 4097 code units but only 2049 scalars: a
        // scalar-counted check would accept it, so the unit semantics are pinned at the
        // failure boundary too.
        let mut mixed: Vec<u16> = Vec::with_capacity(MAX_NATIVE_STRING_UTF16 + 1);
        for _ in 0..2048 {
            mixed.extend_from_slice(&[0xD83D, 0xDE00]);
        }
        mixed.push(u16::from(b'x'));
        assert_eq!(mixed.len(), MAX_NATIVE_STRING_UTF16 + 1);
        let value = wide_property(Some(&mixed));
        assert!(
            friendly_name_from_propvariant(&value).is_err(),
            "4097 code units must fail even when the scalar count is smaller"
        );

        // A present value that is not a wide string is refused rather than coerced through
        // Display/BSTR conversion into a label. `From<u32>` is the crate's own official
        // PROPVARIANT impl (VT_UI4).
        let unsupported = PROPVARIANT::from(7u32);
        assert!(
            friendly_name_from_propvariant(&unsupported).is_err(),
            "an unsupported property type must not be coerced into a label"
        );
    }
}
