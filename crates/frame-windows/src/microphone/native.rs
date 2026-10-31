// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! WASAPI microphone capture internals (FRAME-0105, ADR 0007).
//!
//! One fresh joined MTA worker owns COM, the `IMMDevice`, the `IAudioClient`, the
//! `IAudioCaptureClient`, the event handle and every stream call. No COM object, native
//! pointer, buffer or handle escapes this module: the only values that cross a module
//! boundary are plain owned Rust data (endpoint IDs, names, PCM and bounded numbers).
//!
//! Capture is shared-mode and event-driven. Microphones use capture endpoints;
//! explicit desktop loopback uses render endpoints with AUDCLNT_STREAMFLAGS_LOOPBACK. The device mix
//! format is used exactly as reported by the OS; that allocation is owned by an RAII guard
//! which outlives `IAudioClient::Initialize`, and every field the conversion depends on is
//! validated before any pointer is read. A native buffer lives only between its successful
//! `GetBuffer` and its explicit `ReleaseBuffer`, which the same worker performs on every
//! path including conversion failure and unwind.
#![allow(unsafe_code)]

use super::format::DeviceFormat;
use super::lifecycle::{CaptureLifecycle, CaptureStream};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use std::num::{NonZeroU32, NonZeroU64};
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_E_ENDPOINT_CREATE_FAILED, AUDCLNT_E_RESOURCES_INVALIDATED,
    AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, EDataFlow, IAudioCaptureClient,
    IAudioClient, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::core::PWSTR;

/// The largest number of already-available packets one drain batch consumes before the
/// worker re-checks cancellation.
pub(crate) const MAX_DRAIN_BATCH: u32 = 32;

/// A capture endpoint as the OS reports it, in plain owned Rust data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeEndpoint {
    /// The full opaque endpoint ID from `IMMDevice::GetId`, never parsed or truncated.
    pub(crate) endpoint_id: String,
    /// A best-effort readable label from the device property store.
    pub(crate) friendly_name: Option<String>,
}

/// The accepted classification of a native audio status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioFailure {
    /// The app is not allowed to use the endpoint.
    AccessDenied,
    /// The endpoint or its resources are gone or invalidated.
    ResourceLost,
    /// The device mix format is well formed but outside the proof scope.
    UnsupportedFormat,
    /// The endpoint is held exclusively by something else.
    DeviceInUse,
    /// Anything else is an unexpected native failure.
    Other,
}

/// Classifies a native HRESULT using the official binding constants only.
///
/// The values are never transcribed: they come from the installed `windows` 0.61.3
/// bindings, so a wrong handwritten number cannot silently misclassify a failure.
pub(crate) fn classify_audio_error(code: i32) -> AudioFailure {
    if code == E_ACCESSDENIED.0 {
        AudioFailure::AccessDenied
    } else if code == AUDCLNT_E_DEVICE_INVALIDATED.0
        || code == AUDCLNT_E_RESOURCES_INVALIDATED.0
        || code == AUDCLNT_E_ENDPOINT_CREATE_FAILED.0
    {
        AudioFailure::ResourceLost
    } else if code == AUDCLNT_E_DEVICE_IN_USE.0 {
        AudioFailure::DeviceInUse
    } else if code == AUDCLNT_E_UNSUPPORTED_FORMAT.0 {
        AudioFailure::UnsupportedFormat
    } else {
        AudioFailure::Other
    }
}

fn native_failure(api: &str, code: i32, context: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!("{api} failed with HRESULT 0x{code:08X} ({context})."),
        recovery: RecoveryStatus::NoRecording,
        action: "Check the microphone, its driver and the Windows audio service, then retry."
            .into(),
    }
}

fn source_lost(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Re-select the microphone in recording settings and start a new session.".into(),
    }
}

fn permission_denied(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::PermissionDenied,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Allow microphone access for this app in Windows Settings, then retry.".into(),
    }
}

fn unsupported_format(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Select a device whose mix format is in the supported proof scope.".into(),
    }
}

/// Maps a WASAPI failure onto the accepted error codes, keeping API, code and context.
fn map_audio_error(api: &str, error: windows::core::Error, context: &str) -> RecorderError {
    let code = error.code().0;
    match classify_audio_error(code) {
        AudioFailure::AccessDenied => permission_denied(format!(
            "{api} was denied access (HRESULT 0x{code:08X}, {context}); microphone privacy settings or a device policy refused this app."
        )),
        AudioFailure::ResourceLost => source_lost(format!(
            "{api} reported that the selected endpoint or its resources are no longer available (HRESULT 0x{code:08X}, {context})."
        )),
        AudioFailure::DeviceInUse => source_lost(format!(
            "{api} reported that the selected endpoint is in use exclusively (HRESULT 0x{code:08X}, {context})."
        )),
        AudioFailure::UnsupportedFormat => unsupported_format(format!(
            "{api} rejected the device mix format (HRESULT 0x{code:08X}, {context})."
        )),
        AudioFailure::Other => native_failure(api, code, context),
    }
}

/// Combines a primary failure with the cleanup failure that followed it, keeping both.
pub(crate) fn combine(primary: RecorderError, cleanup: RecorderError) -> RecorderError {
    RecorderError {
        code: primary.code,
        message: format!(
            "{} Native cleanup also failed: {}",
            primary.message, cleanup.message
        ),
        recovery: primary.recovery,
        action: format!("{} Then: {}", primary.action, cleanup.action),
    }
}

/// Converts a raw performance counter value into the 100 ns units WASAPI reports.
///
/// ADR 0007 requires checked `u128` arithmetic: a long-uptime counter multiplied by
/// 10,000,000 overflows `u64` even though the converted result fits, so the widening
/// happens before the multiply and the result is narrowed with a checked conversion.
pub(crate) fn qpc_to_hundred_ns(ticks: u64, frequency: NonZeroU64) -> Option<u64> {
    let hundred_ns = u128::from(ticks).checked_mul(10_000_000u128)? / u128::from(frequency.get());
    u64::try_from(hundred_ns).ok()
}

/// The bounded wait decision for one packet: already-available data never waits.
///
/// A pending packet always reports zero wait, so draining buffered packets never blocks on
/// the next event; only a genuinely empty stream waits, and only within the finite slice
/// the caller passes.
pub(crate) fn next_wait_ms(pending_frames: u32, wait_slice_ms: u32) -> u32 {
    if pending_frames > 0 { 0 } else { wait_slice_ms }
}

/// Validates one packet's frame count against the device buffer and the proof cap.
pub(crate) fn validate_packet_frames(
    frames: u32,
    buffer_frames: u32,
    sample_rate: NonZeroU32,
) -> Result<(), RecorderError> {
    if frames == 0 {
        return Err(native_failure(
            "IAudioCaptureClient::GetBuffer",
            -1,
            "an empty packet exposes no usable timestamp or data",
        ));
    }
    let cap = super::format::packet_frame_cap(sample_rate);
    if frames > buffer_frames || frames > cap {
        return Err(native_failure(
            "IAudioCaptureClient::GetBuffer",
            -1,
            "the packet exceeds the device buffer or the accepted 100 ms frame bound",
        ));
    }
    Ok(())
}

/// RAII: `CoUninitialize` exactly once for the worker that initialized COM.
struct ComApartment;

impl ComApartment {
    fn initialize_mta() -> Result<Self, RecorderError> {
        // SAFETY: `CoInitializeEx` is called once on this worker with a null reserved
        // pointer; the guard pairs it with `CoUninitialize` on the same thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| map_audio_error("CoInitializeEx(MTA)", error, "COM apartment"))?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful `CoInitializeEx` above on this same thread.
        unsafe { CoUninitialize() };
    }
}

/// Runs `body` on this thread inside a fresh joined MTA apartment.
pub(crate) fn with_apartment<T>(
    body: impl FnOnce() -> Result<T, RecorderError>,
) -> Result<T, RecorderError> {
    let _apartment = ComApartment::initialize_mta()?;
    body()
}

/// The device mix format allocation returned by `IAudioClient::GetMixFormat`.
///
/// The OS allocates this with the COM task allocator and documents the caller as its
/// owner. This guard owns it from the moment it is returned until it is dropped, which is
/// after `IAudioClient::Initialize` returned, so no freed pointer is ever passed to the
/// client and no failure path can leak it.
pub(crate) struct OwnedMixFormat {
    pointer: *mut WAVEFORMATEX,
    /// Test-only per-instance observer invoked immediately after the real release, so a
    /// test can prove the release happened without skipping or replacing it.
    #[cfg(test)]
    observer: Option<Box<dyn FnOnce()>>,
}

impl OwnedMixFormat {
    /// Adopts a non-null mix format pointer returned by the OS.
    fn adopt(pointer: *mut WAVEFORMATEX) -> Result<Self, RecorderError> {
        if pointer.is_null() {
            return Err(native_failure(
                "IAudioClient::GetMixFormat",
                -1,
                "the device reported no mix format",
            ));
        }
        Ok(Self {
            pointer,
            #[cfg(test)]
            observer: None,
        })
    }

    /// The pointer handed to `Initialize` while this guard is alive.
    pub(crate) fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.pointer
    }

    /// The validated description of this layout.
    pub(crate) fn describe(&self) -> Result<DeviceFormat, RecorderError> {
        read_mix_format(self.pointer)
    }

    /// Adopts a pointer together with a release observer (tests only).
    #[cfg(test)]
    fn adopt_with_observer(
        pointer: *mut WAVEFORMATEX,
        observer: Box<dyn FnOnce()>,
    ) -> Result<Self, RecorderError> {
        let mut owned = Self::adopt(pointer)?;
        owned.observer = Some(observer);
        Ok(owned)
    }
}

/// Initializes the capture client with the device mix format, owning that allocation for
/// exactly the duration of the injected `Initialize` call.
///
/// Production passes the official `IAudioClient::Initialize` call with unchanged
/// parameters. The allocation is validated first, stays alive across the callback, and is
/// released exactly once when this function returns, whether the callback succeeded,
/// returned an error or unwound. The returned layout has no adopted capacity yet:
/// production queries `GetBufferSize` and adopts it afterwards, exactly as before.
pub(crate) fn initialize_with_mix_format<T>(
    mix: OwnedMixFormat,
    initialize: impl FnOnce(*const WAVEFORMATEX) -> Result<T, RecorderError>,
) -> Result<(DeviceFormat, T), RecorderError> {
    let declared = mix.describe()?;
    let outcome = initialize(mix.as_ptr());
    // Released here on every path: the allocation is not needed after Initialize.
    outcome.map(|value| (declared, value))
}

impl Drop for OwnedMixFormat {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            // SAFETY: the pointer came from `GetMixFormat`, whose documented owner is the
            // COM task allocator; it is freed exactly once, on the owning MTA thread.
            unsafe { CoTaskMemFree(Some(self.pointer.cast())) };
            self.pointer = std::ptr::null_mut();
        }
        // The observer runs after the real release; it never replaces it.
        #[cfg(test)]
        if let Some(observer) = self.observer.take() {
            observer();
        }
    }
}

/// Releases a string the OS allocated with the COM task allocator.
struct OwnedPwstr(PWSTR);

impl OwnedPwstr {
    fn take(&mut self) -> Option<String> {
        if self.0.is_null() {
            return None;
        }
        // SAFETY: `GetId` returned a NUL-terminated wide string owned by the task
        // allocator; it is read once and then freed by `Drop`.
        unsafe { self.0.to_string() }.ok()
    }
}

impl Drop for OwnedPwstr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from `IMMDevice::GetId`, which documents the COM
            // task allocator as its owner; freeing it once here is the matching release.
            unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) };
            self.0 = PWSTR::null();
        }
    }
}

/// An event handle owned from the moment it is created.
pub(crate) struct OwnedEvent {
    handle: HANDLE,
}

impl OwnedEvent {
    pub(crate) fn create() -> Result<Self, RecorderError> {
        // SAFETY: the event is created here, owned by this guard and closed exactly once.
        let handle = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| map_audio_error("CreateEventW", error, "capture event"))?;
        Ok(Self { handle })
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: this guard owns the handle and closes it exactly once.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
            self.handle = HANDLE::default();
        }
    }
}

/// Reads a device property as an owned string, tolerating every unavailable case.
fn property_string(
    store: &IPropertyStore,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> Option<String> {
    // SAFETY: the store is alive; the returned PROPVARIANT owns whatever the store wrote
    // and is released by its own `Drop`.
    let value: PROPVARIANT = unsafe { store.GetValue(key) }.ok()?;
    // SAFETY: the variant tag is read before the union member, and only the documented
    // `VT_LPWSTR` representation is interpreted.
    let text = unsafe {
        if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            let raw = value.Anonymous.Anonymous.Anonymous.pwszVal;
            if raw.is_null() {
                None
            } else {
                raw.to_string().ok()
            }
        } else {
            None
        }
    };
    text.filter(|text| !text.is_empty())
}

/// Enumerates active capture endpoints on a fresh joined MTA worker.
pub(crate) fn enumerate_endpoints() -> Result<Vec<NativeEndpoint>, RecorderError> {
    let worker = std::thread::Builder::new()
        .name("frame-mic-enumerate".to_string())
        .spawn(|| enumerate_on_mta(eCapture))
        .map_err(|error| RecorderError {
            code: ErrorCode::Internal,
            message: format!("The microphone enumeration worker could not be created: {error}"),
            recovery: RecoveryStatus::NoRecording,
            action: "Retry; a thread could not be created.".into(),
        })?;
    match worker.join() {
        Ok(result) => result,
        Err(_) => Err(RecorderError {
            code: ErrorCode::Internal,
            message: "The microphone enumeration worker panicked.".to_string(),
            recovery: RecoveryStatus::NoRecording,
            action: "Retry; if this repeats, report the enumeration panic.".into(),
        }),
    }
}

pub(crate) fn enumerate_render_endpoints() -> Result<Vec<NativeEndpoint>, RecorderError> {
    std::thread::Builder::new()
        .name("frame-loopback-enumerate".into())
        .spawn(|| enumerate_on_mta(eRender))
        .map_err(|e| native_failure("spawn", -1, &e.to_string()))?
        .join()
        .map_err(|_| native_failure("join", -1, "render enumeration panicked"))?
}

fn enumerate_on_mta(flow: EDataFlow) -> Result<Vec<NativeEndpoint>, RecorderError> {
    let _apartment = ComApartment::initialize_mta()?;
    let enumerator: IMMDeviceEnumerator = unsafe {
        // SAFETY: `MMDeviceEnumerator` is the documented class for this interface and the
        // call is made inside the MTA worker that owns COM.
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }
    .map_err(|error| {
        map_audio_error(
            "CoCreateInstance(MMDeviceEnumerator)",
            error,
            "device enumerator",
        )
    })?;
    let collection: IMMDeviceCollection = unsafe {
        // SAFETY: the enumerator is alive; only active capture endpoints are requested.
        enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
    }
    .map_err(|error| {
        map_audio_error(
            "IMMDeviceEnumerator::EnumAudioEndpoints",
            error,
            "active capture endpoints",
        )
    })?;
    let count = unsafe {
        // SAFETY: the collection is alive and the call only reports its count.
        collection.GetCount()
    }
    .map_err(|error| map_audio_error("IMMDeviceCollection::GetCount", error, "endpoint count"))?;

    let mut endpoints = Vec::new();
    for index in 0..count {
        let device: IMMDevice = unsafe {
            // SAFETY: `index` is below the reported count and the collection is alive.
            collection.Item(index)
        }
        .map_err(|error| map_audio_error("IMMDeviceCollection::Item", error, "endpoint item"))?;
        let mut id = OwnedPwstr(
            unsafe {
                // SAFETY: the device is alive; the returned string is freed by the guard.
                device.GetId()
            }
            .map_err(|error| map_audio_error("IMMDevice::GetId", error, "endpoint identity"))?,
        );
        let Some(endpoint_id) = id.take() else {
            return Err(source_lost(
                "An active capture endpoint reported no endpoint identifier.".to_string(),
            ));
        };
        let friendly_name = unsafe {
            // SAFETY: the device is alive and the store is opened read-only.
            device.OpenPropertyStore(STGM_READ)
        }
        .ok()
        .and_then(|store| {
            property_string(
                &store,
                &windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
            )
        });
        endpoints.push(NativeEndpoint {
            endpoint_id,
            friendly_name,
        });
    }
    Ok(endpoints)
}

/// Read-only mix-format query. No Initialize, Start, event or capture session.
pub(crate) fn probe_endpoint_format(endpoint_id: &str) -> Result<DeviceFormat, RecorderError> {
    with_apartment(|| {
        // SAFETY: official COM bindings; all interfaces live inside this MTA scope.
        let enumerator: IMMDeviceEnumerator = unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }
        .map_err(|e| map_audio_error("CoCreateInstance(MMDeviceEnumerator)", e, "format probe"))?;
        let wide: Vec<u16> = endpoint_id.encode_utf16().chain(Some(0)).collect();
        // SAFETY: wide is a live, null-terminated identifier from source enumeration.
        let device = unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }
            .map_err(|e| map_audio_error("IMMDeviceEnumerator::GetDevice", e, "format probe"))?;
        // SAFETY: the device is alive, with no activation parameters.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| map_audio_error("IMMDevice::Activate(IAudioClient)", e, "format probe"))?;
        // SAFETY: the returned allocation is immediately adopted by its RAII owner.
        let mix = OwnedMixFormat::adopt(
            unsafe { client.GetMixFormat() }
                .map_err(|e| map_audio_error("IAudioClient::GetMixFormat", e, "format probe"))?,
        )?;
        mix.describe()
    })
}

/// Bounded observations of one acquired packet, kept for diagnostics only.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PacketObservation {
    pub(crate) frames: u32,
    pub(crate) device_position: u64,
    pub(crate) qpc_100ns: u64,
    pub(crate) silent: bool,
    pub(crate) discontinuity: bool,
    pub(crate) timestamp_error: bool,
}

/// Bounded native clock evidence retained for diagnostics (ADR 0007).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClockEvidence {
    pub(crate) raw_qpc_ticks: u64,
    pub(crate) qpc_frequency: u64,
    pub(crate) qpc_100ns: u64,
    pub(crate) bracket_ns: u64,
    pub(crate) uncertainty_ns: u64,
}

type SourceCheck = Box<dyn Fn(bool) -> Result<(), RecorderError>>;

/// One live event-driven capture stream on its own MTA worker.
pub(crate) struct NativeCapture {
    // Field Drop order matters: interfaces release before their event handle.
    capture: IAudioCaptureClient,
    client: IAudioClient,
    event: OwnedEvent,
    format: DeviceFormat,
    buffer_frames: u32,
    lifecycle: CaptureLifecycle,
    source_check: Option<SourceCheck>,
    loopback: bool,
    packet_release_failed: std::cell::Cell<bool>,
}

impl NativeCapture {
    /// Is any packet already available? Never waits.
    fn pending_frames(&self) -> Result<u32, RecorderError> {
        unsafe {
            // SAFETY: the capture client is alive on this worker.
            self.capture.GetNextPacketSize()
        }
        .map_err(|error| {
            map_audio_error(
                "IAudioCaptureClient::GetNextPacketSize",
                error,
                "packet size",
            )
        })
    }

    /// Waits once for the capture event within the finite slice.
    fn wait_for_event(&self, wait_ms: u32) -> Result<bool, RecorderError> {
        // SAFETY: the event handle is owned by this stream and stays valid until finish.
        let wait = unsafe { WaitForSingleObject(self.event.handle(), wait_ms) };
        if wait == WAIT_TIMEOUT {
            return Ok(false);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(native_failure(
                "WaitForSingleObject(capture event)",
                wait.0 as i32,
                "unexpected wait result",
            ));
        }
        Ok(true)
    }

    /// Performs the native `GetBuffer`, exposing the acquired packet's raw fields so the
    /// shared acquire -> convert -> release sequencing can be driven without a device.
    fn get_buffer(&self) -> Result<AcquiredNative, RecorderError> {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut count = 0u32;
        let mut flags = 0u32;
        let mut device_position = 0u64;
        let mut qpc_100ns = 0u64;
        // SAFETY: all out-parameters are live initialised locals; the client is alive on
        // this worker and this is the only outstanding acquisition.
        unsafe {
            self.capture.GetBuffer(
                &mut data,
                &mut count,
                &mut flags,
                Some(&mut device_position),
                Some(&mut qpc_100ns),
            )
        }
        .map_err(|error| {
            map_audio_error("IAudioCaptureClient::GetBuffer", error, "acquire packet")
        })?;
        Ok(AcquiredNative {
            data,
            count,
            flags,
            device_position,
            qpc_100ns,
        })
    }

    /// Performs the native `ReleaseBuffer` for exactly one acquired packet.
    fn release_buffer(&self, frames: u32) -> Result<(), RecorderError> {
        // SAFETY: this balances the successful acquisition on this owner thread. The
        // guard invokes it at most once per acquisition, including on error or unwind.
        let result = unsafe { self.capture.ReleaseBuffer(frames) }.map_err(|error| {
            map_audio_error(
                "IAudioCaptureClient::ReleaseBuffer",
                error,
                "release acquired packet",
            )
        });
        if result.is_err() {
            self.packet_release_failed.set(true);
        }
        result
    }

    /// Acquires and converts one packet into `out`, releasing the native buffer on every
    /// path with an explicit, fallible release.
    ///
    /// Already-available packets are drained before any wait, so a buffered stream never
    /// strands packets behind an event that was already consumed. The acquire -> convert
    /// -> release balance itself lives in [`read_one_packet`], which is the tested seam.
    pub(crate) fn next_packet(
        &self,
        out: &mut Vec<f32>,
        wait_slice_ms: u32,
    ) -> Result<Option<PacketObservation>, RecorderError> {
        if let Some(check) = &self.source_check {
            check(false)?;
        }
        if !self.lifecycle.running() {
            return Err(native_failure(
                "IAudioCaptureClient",
                -1,
                "the stream was already finished",
            ));
        }
        let mut pending = self.pending_frames()?;
        let wait_ms = next_wait_ms(pending, wait_slice_ms);
        if wait_ms > 0 {
            if !self.wait_for_event(wait_ms)? {
                return Ok(None);
            }
            pending = self.pending_frames()?;
        }
        if pending == 0 {
            // The event signalled without data, or nothing is available yet: this is not
            // lost audio and exposes no timestamp.
            return Ok(None);
        }
        read_one_packet(
            || self.get_buffer(),
            |frames| self.release_buffer(frames),
            DeviceFormat::convert_into,
            &self.format,
            self.buffer_frames,
            out,
        )
    }

    /// Explicitly stops the stream on the owning MTA thread. Owners drop afterward.
    ///
    /// This is the production cleanup path; `Drop` is only the unwind fallback, so a
    /// native stop failure is reported instead of being swallowed.
    pub(crate) fn finish(&mut self) -> Result<(), RecorderError> {
        self.lifecycle.finish(|| {
            // SAFETY: a successful Start occurred on this worker, with a live
            // client/event. This stop attempt is disarmed before the native call.
            unsafe { self.client.Stop() }
                .map_err(|error| map_audio_error("IAudioClient::Stop", error, "capture stop"))
        })
    }
}

impl CaptureStream for NativeCapture {
    fn packet_releases_succeeded(&self) -> bool {
        !self.packet_release_failed.get()
    }
    fn allows_packet_gaps(&self) -> bool {
        self.loopback
    }
    fn device_position_available(&self) -> bool {
        self.source_check.is_none()
    }

    fn next_packet(
        &self,
        samples: &mut Vec<f32>,
        wait_ms: u32,
    ) -> Result<Option<PacketObservation>, RecorderError> {
        NativeCapture::next_packet(self, samples, wait_ms)
    }

    fn finish(&mut self) -> Result<(), RecorderError> {
        NativeCapture::finish(self)
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        // The unwind fallback: a Stop failure here cannot be reported, so it must never be
        // the only place native cleanup happens.
        let _ = self.finish();
    }
}

/// The raw fields one successful `GetBuffer` exposes before conversion. No COM type
/// escapes: this is plain data the acquire step returns to the shared sequencing.
struct AcquiredNative {
    data: *mut u8,
    count: u32,
    flags: u32,
    device_position: u64,
    qpc_100ns: u64,
}

/// The conversion step's shape: the real [`DeviceFormat::convert_into`] in production,
/// or an injected step in tests that prove the release balance around it.
type ConvertFn = fn(&DeviceFormat, Option<&[u8]>, u32, &mut Vec<f32>) -> Result<(), RecorderError>;

/// The shared acquire -> convert -> release sequencing for one packet (FRAME-0105).
///
/// Exactly one `release` runs for every successful `acquire`, on every path: an empty
/// packet, an over-large or pointer-less packet, a conversion error and a conversion
/// unwind all release exactly once, and a primary failure keeps any release failure beside
/// it. Production supplies the two native calls; tests inject them (and the conversion
/// step) to prove the balance without a device. This is the one place the release
/// invariant lives, not a COM or testing framework.
fn read_one_packet<Acq, Rel>(
    acquire: Acq,
    release: Rel,
    convert: ConvertFn,
    format: &DeviceFormat,
    buffer_frames: u32,
    out: &mut Vec<f32>,
) -> Result<Option<PacketObservation>, RecorderError>
where
    Acq: FnOnce() -> Result<AcquiredNative, RecorderError>,
    Rel: Fn(u32) -> Result<(), RecorderError>,
{
    let raw = acquire()?;
    let count = raw.count;
    // The acquisition is balanced exactly once on every path below, including a
    // conversion failure and an unwind, and the release result is reported.
    let guard = AcquiredPacket::new(move || release(count));
    if count == 0 {
        return match guard.release() {
            Ok(()) => Ok(None),
            Err(error) => Err(error),
        };
    }
    if let Err(error) = validate_packet_frames(count, buffer_frames, format.sample_rate()) {
        return match guard.release() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(combine(error, cleanup)),
        };
    }

    let silent = (raw.flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
    let discontinuity = (raw.flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;
    let timestamp_error = (raw.flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0;

    // Only the SILENT flag permits synthesised zeros. A nonempty packet without that
    // flag must carry a readable buffer: a null pointer there is invalid, not silence.
    if !silent && raw.data.is_null() {
        let error = native_failure(
            "IAudioCaptureClient::GetBuffer",
            -1,
            "a nonempty packet without the SILENT flag reported no data pointer",
        );
        return match guard.release() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(combine(error, cleanup)),
        };
    }
    let bytes = if silent {
        None
    } else {
        match format.bytes_for_frames(count) {
            Ok(length) => {
                // SAFETY: the acquire step reported `count` valid frames for this packet,
                // the frame bound is validated, and the format proves the exact byte length
                // those frames occupy. The buffer outlives the conversion below.
                Some(unsafe { std::slice::from_raw_parts(raw.data, length) })
            }
            Err(error) => {
                return match guard.release() {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(combine(error, cleanup)),
                };
            }
        }
    };
    let conversion = convert(format, bytes, count, out);
    let released = guard.release();
    match (conversion, released) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => return Err(primary),
        (Ok(()), Err(cleanup)) => return Err(cleanup),
        (Err(primary), Err(cleanup)) => return Err(combine(primary, cleanup)),
    }
    Ok(Some(PacketObservation {
        frames: count,
        device_position: raw.device_position,
        qpc_100ns: raw.qpc_100ns,
        silent,
        discontinuity,
        timestamp_error,
    }))
}

/// Balances one successful `GetBuffer` with exactly one `ReleaseBuffer`.
struct AcquiredPacket<F: FnOnce() -> Result<(), RecorderError>> {
    release: Option<F>,
}

impl<F: FnOnce() -> Result<(), RecorderError>> AcquiredPacket<F> {
    fn new(release: F) -> Self {
        Self {
            release: Some(release),
        }
    }

    /// Performs the single release and reports a native failure to the caller.
    fn release(mut self) -> Result<(), RecorderError> {
        // Disarm BEFORE calling: failure or unwind must not retry ReleaseBuffer.
        self.release.take().map_or(Ok(()), |release| release())
    }
}

impl<F: FnOnce() -> Result<(), RecorderError>> Drop for AcquiredPacket<F> {
    fn drop(&mut self) {
        // Only reached on an unwind between acquisition and the explicit release: the
        // acquisition must still be balanced exactly once.
        if let Some(release) = self.release.take() {
            let _ = release();
        }
    }
}

/// A prepared capture that has not started streaming yet.
pub(crate) struct PreparedCapture {
    pub(crate) format: DeviceFormat,
    capture: NativeCapture,
}

impl PreparedCapture {
    /// The device format actually in force.
    pub(crate) fn format(&self) -> DeviceFormat {
        self.format
    }

    /// Starts streaming before the worker signals readiness, after its last check.
    pub(crate) fn start(
        self,
        checkpoint: impl Fn() -> Result<(), RecorderError>,
    ) -> Result<NativeCapture, RecorderError> {
        checkpoint()?;
        let mut capture = self.capture;
        if let Some(check) = &capture.source_check {
            check(true)?;
        }
        checkpoint()?;
        capture.lifecycle.start(|| {
            // SAFETY: the client is initialized and the event is already set.
            unsafe { capture.client.Start() }
                .map_err(|error| map_audio_error("IAudioClient::Start", error, "capture start"))
        })?;
        Ok(capture)
    }
}

/// Adopt an initialized client into the accepted packet and cleanup implementation.
/// The caller owns all native values on this MTA worker; nothing crosses threads.
pub(crate) fn adopt_initialized_capture(
    event: OwnedEvent,
    client: IAudioClient,
    declared: DeviceFormat,
    source_check: SourceCheck,
) -> Result<PreparedCapture, RecorderError> {
    // SAFETY: the caller has successfully initialized this client on this worker.
    let frames = unsafe { client.GetBufferSize() }
        .map_err(|e| map_audio_error("IAudioClient::GetBufferSize", e, "process loopback"))?;
    let format = declared.with_buffer_frames(frames)?;
    // SAFETY: event ownership is transferred to NativeCapture on success.
    unsafe { client.SetEventHandle(event.handle()) }
        .map_err(|e| map_audio_error("IAudioClient::SetEventHandle", e, "process loopback"))?;
    // SAFETY: the initialized client owns this service; it stays on this worker.
    let capture = unsafe { client.GetService::<IAudioCaptureClient>() }.map_err(|e| {
        map_audio_error(
            "IAudioClient::GetService(IAudioCaptureClient)",
            e,
            "process loopback",
        )
    })?;
    Ok(PreparedCapture {
        format,
        capture: NativeCapture {
            capture,
            client,
            event,
            format,
            buffer_frames: frames,
            lifecycle: CaptureLifecycle::default(),
            source_check: Some(source_check),
            loopback: false,
            packet_release_failed: std::cell::Cell::new(false),
        },
    })
}

/// Reads and validates the fields of a mix format without reading past `cbSize`.
fn read_mix_format(pointer: *mut WAVEFORMATEX) -> Result<DeviceFormat, RecorderError> {
    // SAFETY: the caller holds the RAII owner of this allocation and checked it is
    // non-null; it points at a `WAVEFORMATEX` (or a `WAVEFORMATEXTENSIBLE` whose leading
    // member is one).
    let base = unsafe { *pointer };
    let tag = u32::from(base.wFormatTag);
    if base.nBlockAlign == 0 || base.nChannels == 0 || base.nSamplesPerSec == 0 {
        return Err(native_failure(
            "IAudioClient::GetMixFormat",
            -1,
            "the device mix format has a zero alignment, channel count or sample rate",
        ));
    }
    // A declared average byte rate must agree with rate x alignment, or the layout is
    // internally inconsistent and cannot be read safely.
    let expected_rate = u64::from(base.nSamplesPerSec)
        .checked_mul(u64::from(base.nBlockAlign))
        .ok_or_else(|| {
            native_failure(
                "IAudioClient::GetMixFormat",
                -1,
                "the device mix format byte rate overflows",
            )
        })?;
    if u64::from(base.nAvgBytesPerSec) != expected_rate {
        return Err(native_failure(
            "IAudioClient::GetMixFormat",
            -1,
            "the device mix format's average byte rate disagrees with its rate and alignment",
        ));
    }

    let extensible = tag == WAVE_FORMAT_EXTENSIBLE;
    if extensible && base.cbSize < 22 {
        // A short extensible header is malformed, not merely unsupported: reading its body
        // would run past the allocation.
        return Err(native_failure(
            "IAudioClient::GetMixFormat",
            -1,
            "the device declared an extensible mix format with a truncated body",
        ));
    }
    let (valid_bits, is_float, channel_mask) = if extensible {
        // SAFETY: `cbSize >= 22` proves the extensible body is present, and the mapping is
        // read exactly as the OS declared it.
        let full = unsafe { *(pointer as *const WAVEFORMATEXTENSIBLE) };
        let valid = unsafe { full.Samples.wValidBitsPerSample };
        let subformat = full.SubFormat;
        let float = subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        let pcm = subformat == KSDATAFORMAT_SUBTYPE_PCM;
        if !float && !pcm {
            return Err(unsupported_format(format!(
                "The device mix format declares the subformat {:?}, which this proof does not support.",
                subformat
            )));
        }
        (valid, float, Some(full.dwChannelMask))
    } else {
        match tag {
            tag if tag == WAVE_FORMAT_IEEE_FLOAT => (base.wBitsPerSample, true, None),
            tag if tag == WAVE_FORMAT_PCM => (base.wBitsPerSample, false, None),
            other => {
                return Err(unsupported_format(format!(
                    "The device mix format uses wFormatTag {other}, which this proof does not support."
                )));
            }
        }
    };

    if let Some(mask) = channel_mask {
        // Only the layouts this proof converts are accepted; an unsupported mask is not a
        // malformed format, so it is reported as unsupported.
        let channel_count = base.nChannels;
        let supported = match channel_count {
            1 => mask == 0 || mask == 0x4,
            2 => mask == 0 || mask == 0x3,
            _ => false,
        };
        if !supported {
            return Err(unsupported_format(format!(
                "The device mix format declares channel mask 0x{mask:X} for {} channel(s), which this proof does not support.",
                channel_count
            )));
        }
    }

    DeviceFormat::from_layout(crate::microphone::format::FormatDescription {
        sample_rate: base.nSamplesPerSec,
        channels: base.nChannels,
        bits_per_sample: base.wBitsPerSample,
        block_align: base.nBlockAlign,
        valid_bits,
        is_float: Some(is_float),
        extensible,
    })
}

/// Prepares, initialises and starts one selected endpoint's stream on this worker.
///
/// The exact endpoint is resolved again here; a missing endpoint is `SourceLost` and is
/// never replaced by the default device.
pub(crate) fn prepare_capture(
    endpoint_id: &str,
    checkpoint: &dyn Fn() -> Result<(), RecorderError>,
) -> Result<PreparedCapture, RecorderError> {
    prepare_endpoint(endpoint_id, false, checkpoint)
}

pub(crate) fn prepare_loopback(
    endpoint_id: &str,
    checkpoint: &dyn Fn() -> Result<(), RecorderError>,
) -> Result<PreparedCapture, RecorderError> {
    prepare_endpoint(endpoint_id, true, checkpoint)
}

fn prepare_endpoint(
    endpoint_id: &str,
    loopback: bool,
    checkpoint: &dyn Fn() -> Result<(), RecorderError>,
) -> Result<PreparedCapture, RecorderError> {
    let flow = if loopback { eRender } else { eCapture };
    checkpoint()?;
    let enumerator: IMMDeviceEnumerator = unsafe {
        // SAFETY: called on the MTA worker that owns COM.
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }
    .map_err(|error| {
        map_audio_error(
            "CoCreateInstance(MMDeviceEnumerator)",
            error,
            "device enumerator",
        )
    })?;
    checkpoint()?;
    let collection: IMMDeviceCollection = unsafe {
        // SAFETY: the enumerator is alive; only active capture endpoints are requested.
        enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
    }
    .map_err(|error| {
        map_audio_error(
            "IMMDeviceEnumerator::EnumAudioEndpoints",
            error,
            "active capture endpoints",
        )
    })?;
    checkpoint()?;
    let count = unsafe {
        // SAFETY: the collection is alive.
        collection.GetCount()
    }
    .map_err(|error| map_audio_error("IMMDeviceCollection::GetCount", error, "endpoint count"))?;

    let mut selected: Option<IMMDevice> = None;
    for index in 0..count {
        checkpoint()?;
        let device: IMMDevice = unsafe {
            // SAFETY: `index` is below the reported count.
            collection.Item(index)
        }
        .map_err(|error| map_audio_error("IMMDeviceCollection::Item", error, "endpoint item"))?;
        checkpoint()?;
        let mut id = OwnedPwstr(
            unsafe {
                // SAFETY: the device is alive.
                device.GetId()
            }
            .map_err(|error| map_audio_error("IMMDevice::GetId", error, "endpoint identity"))?,
        );
        if id.take().as_deref() == Some(endpoint_id) {
            selected = Some(device);
            break;
        }
    }
    let device = selected.ok_or_else(|| {
        source_lost(format!(
            "The selected microphone ({}…) is not an active capture endpoint any more.",
            endpoint_id.chars().take(24).collect::<String>()
        ))
    })?;

    // Declare the event before client interfaces so every partial-preparation exit
    // drops those interfaces before closing a handle registered with the client.
    checkpoint()?;
    let event = OwnedEvent::create()?;
    checkpoint()?;
    let client: IAudioClient = unsafe {
        // SAFETY: the device is alive and no activation parameters are used for a shared
        // mode audio client.
        device.Activate(CLSCTX_ALL, None)
    }
    .map_err(|error| map_audio_error("IMMDevice::Activate(IAudioClient)", error, "audio client"))?;

    // The OS allocation is owned from this moment until after Initialize returns.
    checkpoint()?;
    let mix = OwnedMixFormat::adopt(
        unsafe {
            // SAFETY: the client is alive; the returned allocation is owned by the guard.
            client.GetMixFormat()
        }
        .map_err(|error| {
            map_audio_error("IAudioClient::GetMixFormat", error, "device mix format")
        })?,
    )?;
    checkpoint()?;
    // Shared mode, event-driven: zero duration and periodicity let the engine choose, and
    // the tested helper owns the mix-format allocation across exactly this call.
    let (declared, ()) = initialize_with_mix_format(mix, |format_pointer| {
        unsafe {
            // SAFETY: the client is alive and `format_pointer` is owned by the helper for
            // the whole duration of this call.
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | if loopback {
                        AUDCLNT_STREAMFLAGS_LOOPBACK
                    } else {
                        0
                    },
                0,
                0,
                format_pointer,
                None,
            )
        }
        .map_err(|error| {
            map_audio_error(
                "IAudioClient::Initialize",
                error,
                "shared event-driven capture",
            )
        })
    })?;

    checkpoint()?;
    let buffer_frames = unsafe {
        // SAFETY: the client is initialised.
        client.GetBufferSize()
    }
    .map_err(|error| map_audio_error("IAudioClient::GetBufferSize", error, "device buffer size"))?;
    // The real native capacity replaces the placeholder: every packet read is validated
    // against what the device actually reported.
    let format = declared.with_buffer_frames(buffer_frames)?;

    checkpoint()?;
    if let Err(error) = unsafe {
        // SAFETY: the client is initialised and the handle is owned by `event`.
        client.SetEventHandle(event.handle())
    } {
        return Err(map_audio_error(
            "IAudioClient::SetEventHandle",
            error,
            "capture event",
        ));
    }

    checkpoint()?;
    let capture_client: IAudioCaptureClient = match unsafe {
        // SAFETY: the client is initialised; the capture client belongs to this worker.
        client.GetService()
    } {
        Ok(capture) => capture,
        Err(error) => {
            // `event` is dropped here, closing the handle exactly once.
            return Err(map_audio_error(
                "IAudioClient::GetService(IAudioCaptureClient)",
                error,
                "capture client",
            ));
        }
    };

    // Still prepared: cancellation drops owners without attempting Stop.
    checkpoint()?;
    let prepared = PreparedCapture {
        format,
        capture: NativeCapture {
            event,
            client,
            capture: capture_client,
            format,
            buffer_frames,
            lifecycle: CaptureLifecycle::default(),
            source_check: None,
            loopback,
            packet_release_failed: std::cell::Cell::new(false),
        },
    };
    // The event is set before Start, as ADR 0007 requires.
    unsafe {
        // SAFETY: the handle is valid and owned by the prepared capture.
        let _ = SetEvent(prepared.capture.event.handle());
    }
    Ok(prepared)
}

/// The QPC frequency, or `None` when the OS refuses to report it.
pub(crate) fn qpc_frequency() -> Option<NonZeroU64> {
    let mut frequency = 0i64;
    // SAFETY: the out-parameter is a live local.
    if unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
        .is_err()
    {
        return None;
    }
    NonZeroU64::new(u64::try_from(frequency).ok()?)
}

/// The bracketed `QueryPerformanceCounter` sample used to build a `ClockAnchor`.
///
/// ADR 0007: at most eight bracketed samples of the shared clock and QPC; the narrowest
/// bracket at or below 1 ms is selected and raw QPC is converted to the 100 ns units
/// WASAPI reports with checked `u128` arithmetic. The raw ticks, frequency, bracket and
/// uncertainty travel back with the anchor so the session can keep bounded native timing
/// evidence instead of discarding it.
pub(crate) fn qpc_anchor(
    clock: &frame_clock::MasterClock,
    qpc_frequency: NonZeroU64,
) -> Option<(frame_clock::ClockAnchor, ClockEvidence)> {
    use frame_clock::ClockAnchor;
    let mut best: Option<(u64, ClockAnchor, ClockEvidence)> = None;
    for _ in 0..8 {
        let before = clock.now();
        let mut counter = 0i64;
        // SAFETY: the out-parameter is a live local and the call has no other effect.
        if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut counter) }
            .is_err()
        {
            return None;
        }
        let after = clock.now();
        let ticks = u64::try_from(counter).ok()?;
        let hundred_ns = qpc_to_hundred_ns(ticks, qpc_frequency)?;
        let Some(anchor) =
            ClockAnchor::from_sample(hundred_ns, NonZeroU64::new(10_000_000)?, before, after)
        else {
            continue;
        };
        let span = after.0.checked_sub(before.0)?;
        if span > 1_000_000 {
            // Above the accepted bracket width: keep looking rather than anchor loosely.
            continue;
        }
        let evidence = ClockEvidence {
            raw_qpc_ticks: ticks,
            qpc_frequency: qpc_frequency.get(),
            qpc_100ns: hundred_ns,
            bracket_ns: span,
            uncertainty_ns: anchor.uncertainty_ns(),
        };
        match best {
            Some((width, _, _)) if width <= span => {}
            _ => best = Some((span, anchor, evidence)),
        }
    }
    best.map(|(_, anchor, evidence)| (anchor, evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::Win32::System::Com::CoTaskMemAlloc;

    #[test]
    fn review_explicit_packet_release_runs_once_even_when_it_fails() {
        for fails in [false, true] {
            let calls = AtomicUsize::new(0);
            let guard = AcquiredPacket::new(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                if fails {
                    Err(native_failure("ReleaseBuffer", -1, "injected"))
                } else {
                    Ok(())
                }
            });
            assert_eq!(guard.release().is_err(), fails);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn review_packet_guard_releases_once_on_early_error_and_unwind() {
        let calls = AtomicUsize::new(0);
        let result: Result<(), RecorderError> = {
            let _guard = AcquiredPacket::new(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            Err(native_failure("conversion", -1, "injected"))
        };
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let result = std::panic::catch_unwind(|| {
            let _guard = AcquiredPacket::new(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            panic!("injected conversion unwind");
        });
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn review_panicking_release_is_not_retried_by_drop() {
        let calls = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(|| {
            let guard = AcquiredPacket::new(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("injected release unwind");
            });
            let _ = guard.release();
        });
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Allocates a valid device mix format with the SAME allocator the OS uses.
    fn valid_mix_format(observer: impl FnOnce() + 'static) -> OwnedMixFormat {
        let layout = std::alloc::Layout::new::<WAVEFORMATEX>();
        // SAFETY: the task allocator provides the memory the production free path expects;
        // `OwnedMixFormat` releases it exactly once with `CoTaskMemFree`.
        let pointer = unsafe { CoTaskMemAlloc(layout.size()) }.cast::<WAVEFORMATEX>();
        assert!(!pointer.is_null(), "CoTaskMemAlloc failed");
        // SAFETY: the allocation is at least `WAVEFORMATEX`-sized and uninitialised.
        unsafe {
            std::ptr::write(
                pointer,
                WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_PCM as u16,
                    nChannels: 1,
                    nSamplesPerSec: 48_000,
                    nAvgBytesPerSec: 96_000,
                    nBlockAlign: 2,
                    wBitsPerSample: 16,
                    cbSize: 0,
                },
            );
        }
        OwnedMixFormat::adopt_with_observer(pointer, Box::new(observer)).expect("adopt fixture")
    }

    #[test]
    fn the_initialization_helper_keeps_the_format_alive_through_a_successful_call() {
        let frees = std::sync::Arc::new(AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&frees);
        let mix = valid_mix_format(move || {
            observed.fetch_add(1, Ordering::AcqRel);
        });
        let (declared, ()) =
            initialize_with_mix_format(mix, |pointer| -> Result<(), RecorderError> {
                // The allocation must still be readable while the callback runs, and nothing
                // may have been released yet.
                // SAFETY: the helper owns this allocation for the whole call.
                let read_back = unsafe { *pointer };
                let rate = read_back.nSamplesPerSec;
                assert_eq!(rate, 48_000);
                assert_eq!(
                    frees.load(Ordering::Acquire),
                    0,
                    "released during Initialize"
                );
                Ok(())
            })
            .expect("a successful Initialize");
        assert_eq!(declared.sample_rate().get(), 48_000);
        assert_eq!(declared.channels().get(), 1);
        // Capacity stays unadopted here: production adopts GetBufferSize afterwards.
        assert_eq!(declared.buffer_frames(), 0);
        assert_eq!(frees.load(Ordering::Acquire), 1, "exactly one release");
    }

    #[test]
    fn the_initialization_helper_preserves_an_initialize_failure_and_still_releases() {
        let frees = std::sync::Arc::new(AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&frees);
        let mix = valid_mix_format(move || {
            observed.fetch_add(1, Ordering::AcqRel);
        });
        let error = initialize_with_mix_format(mix, |pointer| -> Result<(), RecorderError> {
            // SAFETY: the helper owns this allocation for the whole call.
            let channels = unsafe { *pointer }.nChannels;
            assert_eq!(channels, 1);
            assert_eq!(
                frees.load(Ordering::Acquire),
                0,
                "released during Initialize"
            );
            Err(unsupported_format(
                "injected Initialize rejection".to_string(),
            ))
        })
        .expect_err("the callback error must reach the caller");
        // The original API/code is preserved, not replaced by a cleanup error.
        assert_eq!(error.code, ErrorCode::NotImplemented);
        assert!(error.message.contains("injected Initialize rejection"));
        assert_eq!(frees.load(Ordering::Acquire), 1, "exactly one release");
    }

    #[test]
    fn the_initialization_helper_releases_the_format_when_the_callback_unwinds() {
        let frees = std::sync::Arc::new(AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&frees);
        let mix = valid_mix_format(move || {
            observed.fetch_add(1, Ordering::AcqRel);
        });
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ignored: Result<(DeviceFormat, ()), RecorderError> =
                initialize_with_mix_format(mix, |pointer| -> Result<(), RecorderError> {
                    // SAFETY: the helper owns this allocation for the whole call.
                    let align = unsafe { *pointer }.nBlockAlign;
                    assert_eq!(align, 2);
                    assert_eq!(
                        frees.load(Ordering::Acquire),
                        0,
                        "released during Initialize"
                    );
                    panic!("injected Initialize unwind");
                });
        }));
        assert!(unwound.is_err(), "the unwind must propagate");
        assert_eq!(frees.load(Ordering::Acquire), 1, "exactly one release");
    }

    #[test]
    fn the_real_native_capacity_replaces_the_placeholder() {
        let declared = DeviceFormat::from_parts(
            crate::microphone::format::FormatDescription {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 32,
                block_align: 8,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            4800,
        )
        .unwrap();
        // A declaration is not a capacity; adopting GetBufferSize is what makes an
        // ordinary 480-frame packet legal.
        let adopted = declared.with_buffer_frames(480).unwrap();
        assert_eq!(adopted.buffer_frames(), 480);
        assert!(
            validate_packet_frames(480, adopted.buffer_frames(), adopted.sample_rate()).is_ok()
        );
        assert!(
            validate_packet_frames(481, adopted.buffer_frames(), adopted.sample_rate()).is_err(),
            "a packet beyond the actual capacity must fail"
        );
        let wide = declared.with_buffer_frames(4800).unwrap();
        assert!(
            validate_packet_frames(4801, wide.buffer_frames(), wide.sample_rate()).is_err(),
            "the proof cap still applies when the device buffer is larger"
        );
        assert!(declared.with_buffer_frames(0).is_err());
        assert!(
            declared.with_buffer_frames(4801).is_err(),
            "a device buffer beyond the proof cap is refused"
        );
    }

    #[test]
    fn an_empty_packet_is_not_a_packet_with_a_timestamp() {
        let rate = NonZeroU32::new(48_000).unwrap();
        assert!(validate_packet_frames(0, 4800, rate).is_err());
    }

    #[test]
    fn official_hresult_constants_classify_as_their_documented_meaning() {
        // The values come from the installed bindings; the previously handwritten
        // ENDPOINT_CREATE_FAILED constant is not reused anywhere.
        assert_eq!(
            classify_audio_error(E_ACCESSDENIED.0),
            AudioFailure::AccessDenied
        );
        assert_eq!(
            classify_audio_error(AUDCLNT_E_ENDPOINT_CREATE_FAILED.0),
            AudioFailure::ResourceLost
        );
        assert_eq!(
            classify_audio_error(AUDCLNT_E_RESOURCES_INVALIDATED.0),
            AudioFailure::ResourceLost
        );
        assert_eq!(
            classify_audio_error(AUDCLNT_E_DEVICE_INVALIDATED.0),
            AudioFailure::ResourceLost
        );
        assert_eq!(
            classify_audio_error(AUDCLNT_E_DEVICE_IN_USE.0),
            AudioFailure::DeviceInUse
        );
        assert_eq!(
            classify_audio_error(AUDCLNT_E_UNSUPPORTED_FORMAT.0),
            AudioFailure::UnsupportedFormat
        );
        assert_eq!(
            classify_audio_error(0x8000_4005u32 as i32),
            AudioFailure::Other
        );
        let mapped = map_audio_error(
            "IMMDevice::Activate",
            windows::core::Error::from_hresult(AUDCLNT_E_ENDPOINT_CREATE_FAILED),
            "audio client",
        );
        assert_eq!(mapped.code, ErrorCode::SourceLost);
        assert!(
            mapped.message.contains("0x"),
            "the native code is preserved"
        );
    }

    #[test]
    fn long_uptime_qpc_values_do_not_overflow_the_conversion() {
        let frequency = NonZeroU64::new(10_000_000).unwrap();
        // Ten million seconds of uptime at 10 MHz: ticks x 10,000,000 overflows u64 even
        // though the converted 100 ns result fits comfortably.
        let ticks = 10_000_000u64 * 10_000_000u64;
        assert_eq!(
            qpc_to_hundred_ns(ticks, frequency),
            Some(10_000_000u64 * 10_000_000u64)
        );
        assert_eq!(qpc_to_hundred_ns(0, frequency), Some(0));
        // A fast counter on a long-uptime machine converts the same way.
        let fast = NonZeroU64::new(24_900_000).unwrap();
        assert!(qpc_to_hundred_ns(u64::MAX / 2, fast).is_some());
    }

    #[test]
    fn available_packets_are_drained_before_any_wait() {
        // A stream with data never waits for the event; an empty one waits within the
        // finite slice the worker passes.
        assert_eq!(next_wait_ms(480, 10), 0);
        assert_eq!(next_wait_ms(1, 10), 0);
        assert_eq!(next_wait_ms(0, 10), 10);
    }

    #[test]
    fn a_primary_and_a_cleanup_failure_are_both_preserved() {
        let combined = combine(
            source_lost("primary".to_string()),
            native_failure("IAudioCaptureClient::ReleaseBuffer", -1, "cleanup"),
        );
        assert_eq!(combined.code, ErrorCode::SourceLost);
        assert!(combined.message.contains("primary"));
        assert!(combined.message.contains("Native cleanup also failed"));
    }

    /// A mono IEEE-float32 device format with an adopted native capacity.
    fn mono_float(buffer_frames: u32) -> DeviceFormat {
        DeviceFormat::from_parts(
            crate::microphone::format::FormatDescription {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 32,
                block_align: 4,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            buffer_frames,
        )
        .unwrap()
    }

    /// An injected conversion step that unwinds, to prove the release balance survives it.
    fn panicking_convert(
        _: &DeviceFormat,
        _: Option<&[u8]>,
        _: u32,
        _: &mut Vec<f32>,
    ) -> Result<(), RecorderError> {
        panic!("injected conversion unwind")
    }

    #[test]
    fn the_production_packet_path_releases_the_buffer_exactly_once_on_success() {
        let format = mono_float(480);
        let mut native_bytes = 1.0f32.to_le_bytes().to_vec();
        let data = native_bytes.as_mut_ptr();
        let releases = AtomicUsize::new(0);
        let mut out = Vec::new();
        let observation = read_one_packet(
            || {
                Ok(AcquiredNative {
                    data,
                    count: 1,
                    flags: 0,
                    device_position: 7,
                    qpc_100ns: 9,
                })
            },
            |frames| {
                assert_eq!(frames, 1, "the release is given the acquired frame count");
                releases.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            DeviceFormat::convert_into,
            &format,
            480,
            &mut out,
        )
        .expect("a valid packet converts")
        .expect("one packet is observed");
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "exactly one ReleaseBuffer"
        );
        assert_eq!(observation.frames, 1);
        assert_eq!(observation.device_position, 7);
        assert_eq!(observation.qpc_100ns, 9);
        assert_eq!(out, vec![1.0]);
    }

    #[test]
    fn a_conversion_error_releases_the_buffer_exactly_once() {
        let format = mono_float(480);
        // A non-finite sample makes the real converter refuse the packet.
        let mut native_bytes = f32::NAN.to_le_bytes().to_vec();
        let data = native_bytes.as_mut_ptr();
        let releases = AtomicUsize::new(0);
        let mut out = Vec::new();
        let error = read_one_packet(
            || {
                Ok(AcquiredNative {
                    data,
                    count: 1,
                    flags: 0,
                    device_position: 0,
                    qpc_100ns: 0,
                })
            },
            |_| {
                releases.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            DeviceFormat::convert_into,
            &format,
            480,
            &mut out,
        )
        .expect_err("a non-finite sample is refused");
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "exactly one ReleaseBuffer"
        );
        assert!(error.message.contains("non-finite"), "{}", error.message);
    }

    #[test]
    fn a_conversion_unwind_releases_the_buffer_exactly_once() {
        let format = mono_float(480);
        let mut native_bytes = 0.5f32.to_le_bytes().to_vec();
        let data = native_bytes.as_mut_ptr();
        let releases = AtomicUsize::new(0);
        let mut out = Vec::new();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = read_one_packet(
                || {
                    Ok(AcquiredNative {
                        data,
                        count: 1,
                        flags: 0,
                        device_position: 0,
                        qpc_100ns: 0,
                    })
                },
                |_| {
                    releases.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                panicking_convert,
                &format,
                480,
                &mut out,
            );
        }));
        assert!(unwound.is_err(), "the conversion unwind must propagate");
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "exactly one ReleaseBuffer on unwind"
        );
    }
}
