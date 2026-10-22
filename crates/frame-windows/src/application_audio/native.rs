// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Official process-loopback activation; all COM values remain on the owner MTA.
#![allow(unsafe_code)]
use super::{compare_identity, error, lost};
use crate::microphone::{
    format::{DeviceFormat, FormatDescription},
    native::{OwnedEvent, PreparedCapture, adopt_initialized_capture},
};
use frame_contracts::{AudioSourceId, ErrorCode, RecorderError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, E_INVALIDARG, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CoTaskMemAlloc};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, Interface, Ref, implement};

fn failure(api: &str, native: windows::core::Error) -> RecorderError {
    let code = if native.code() == E_ACCESSDENIED {
        ErrorCode::PermissionDenied
    } else if native.code() == E_INVALIDARG && api == "OpenProcess" {
        ErrorCode::SourceLost
    } else {
        ErrorCode::Internal
    };
    error(
        code,
        format!(
            "{api} failed: HRESULT 0x{:08X} ({native}).",
            native.code().0
        ),
    )
}

struct Process(HANDLE);
impl Process {
    fn open(pid: u32) -> Result<Self, RecorderError> {
        // SAFETY: explicit nonzero PID; only query/synchronize rights, never modification.
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map(Self)
        .map_err(|e| failure("OpenProcess", e))
    }
    fn creation(&self) -> Result<u64, RecorderError> {
        self.alive()?;
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: four writable locals and the retained live process handle.
        unsafe { GetProcessTimes(self.0, &mut creation, &mut exit, &mut kernel, &mut user) }
            .map_err(|e| failure("GetProcessTimes", e))?;
        Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }
    fn alive(&self) -> Result<(), RecorderError> {
        // SAFETY: owned handle has SYNCHRONIZE rights and is retained through Stop.
        let status = unsafe { WaitForSingleObject(self.0, 0) };
        root_wait_result(status.0)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle is owned uniquely by this guard, on this worker.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn root_wait_result(status: u32) -> Result<(), RecorderError> {
    if status == WAIT_TIMEOUT.0 {
        Ok(())
    } else if status == WAIT_OBJECT_0.0 {
        Err(lost(
            "Selected root process exited; descendants are not retained as a replacement source.",
        ))
    } else {
        Err(error(
            ErrorCode::Internal,
            format!("WaitForSingleObject(process) failed with status 0x{status:08X}."),
        ))
    }
}

/// The generated implementation is agile by default. Only a Rust condition event
/// crosses callback threads; no COM interface or raw native owner crosses threads.
#[derive(Default)]
struct CompletionSignal {
    done: Mutex<bool>,
    wake: Condvar,
}
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    signal: Arc<CompletionSignal>,
}
impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        *self.signal.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signal.wake.notify_one();
        Ok(())
    }
}

/// Drain completion even after a cancelled/timed-out checkpoint. This event has
/// finite wait slices but deliberately makes no hard-return-time promise.
fn drain_completion(
    signal: &CompletionSignal,
    checkpoint: &dyn Fn() -> Result<(), RecorderError>,
) -> Result<(), RecorderError> {
    let mut prevented = None;
    loop {
        if prevented.is_none() {
            prevented = checkpoint().err();
        }
        let done = signal.done.lock().unwrap_or_else(|e| e.into_inner());
        if *done {
            break;
        }
        let _ = signal
            .wake
            .wait_timeout(done, Duration::from_millis(10))
            .unwrap_or_else(|e| e.into_inner());
    }
    prevented.map_or_else(checkpoint, Err)
}
fn activation_layers(
    retrieval: windows::core::Result<()>,
    activation: HRESULT,
) -> Result<(), RecorderError> {
    retrieval.map_err(|e| {
        failure(
            "IActivateAudioInterfaceAsyncOperation::GetActivateResult",
            e,
        )
    })?;
    activation
        .ok()
        .map_err(|e| failure("ActivateAudioInterfaceAsync(activation result)", e))
}

fn parameters(pid: u32, exclude: bool) -> Result<PROPVARIANT, RecorderError> {
    let size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    // SAFETY: this allocation is adopted by VT_BLOB below and freed by PropVariantClear.
    let pointer = unsafe { CoTaskMemAlloc(size) }.cast::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    if pointer.is_null() {
        return Err(error(
            ErrorCode::Internal,
            "CoTaskMemAlloc(activation parameters) failed.",
        ));
    }
    // SAFETY: allocation has the exact size/alignment of the official structure.
    unsafe {
        pointer.write(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: if exclude {
                        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                    } else {
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                    },
                },
            },
        });
    }
    let mut value = PROPVARIANT::default();
    // SAFETY: VT_BLOB selects blob; it is a task-allocated unique allocation.
    unsafe {
        (*value.Anonymous.Anonymous).vt = VT_BLOB;
        (*value.Anonymous.Anonymous).Anonymous.blob = BLOB {
            cbSize: size as u32,
            pBlobData: pointer.cast(),
        };
    }
    Ok(value)
}
pub(crate) fn build_number() -> Result<u32, RecorderError> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: official binding, sized writable structure, no compatibility manifest query.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.0 < 0 {
        return Err(error(
            ErrorCode::Internal,
            format!("RtlGetVersion failed: NTSTATUS 0x{:08X}.", status.0),
        ));
    }
    Ok(version.dwBuildNumber)
}
pub(crate) fn prepare(
    source: &AudioSourceId,
    exclude: bool,
    checkpoint: &dyn Fn() -> Result<(), RecorderError>,
) -> Result<PreparedCapture, RecorderError> {
    checkpoint()?;
    let build = build_number()?;
    if build < 20348 {
        return Err(error(
            ErrorCode::NotImplemented,
            format!(
                "Process loopback requires build 20348+; this build is {build}. The generic activation page says 20438; actual activation is checked."
            ),
        ));
    }
    let identity = crate::application_sessions::capture_identity(source)?;
    let root = Process::open(identity.0)?;
    compare_identity(identity, (identity.0, root.creation()?))?;
    compare_identity(
        identity,
        crate::application_sessions::capture_identity(source)?,
    )?;
    checkpoint()?;
    // Declare the capture event before clients so partial-preparation errors drop
    // native interfaces before their event. The activation condition event is Arc-owned.
    let event = OwnedEvent::create()?;
    let blob = parameters(identity.0, exclude)?;
    let signal = Arc::new(CompletionSignal::default());
    let callback: IActivateAudioInterfaceCompletionHandler = Completion {
        signal: signal.clone(),
    }
    .into();
    // SAFETY: all parameters are official SDK owners and stay alive until completion
    // is drained, including after cancellation. The generated handler is agile.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&blob),
            &callback,
        )
    }
    .map_err(|e| failure("ActivateAudioInterfaceAsync", e))?;
    let permitted = drain_completion(&signal, checkpoint);
    let mut activation = HRESULT(0);
    let mut unknown = None;
    // SAFETY: completion has signalled; owner retrieves both native result layers.
    let retrieval = unsafe { operation.GetActivateResult(&mut activation, &mut unknown) };
    let result = activation_layers(retrieval, activation);
    // Explicit order documents that pending callback state never outlives its blob.
    drop(operation);
    drop(callback);
    drop(blob);
    match (permitted, result) {
        (Err(primary), Err(cleanup)) => {
            return Err(crate::microphone::native::combine(primary, cleanup));
        }
        (Err(e), _) | (_, Err(e)) => return Err(e),
        _ => {}
    }
    let client: IAudioClient = unknown
        .ok_or_else(|| {
            error(
                ErrorCode::Internal,
                "Activation succeeded without an interface.",
            )
        })?
        .cast()
        .map_err(|e| failure("QueryInterface(IAudioClient)", e))?;
    checkpoint()?;
    let wave = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 192_000,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    // SAFETY: initialized on the owner MTA with an explicitly requested fixed format.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            &wave,
            None,
        )
    }
    .map_err(|e| {
        failure(
            "IAudioClient::Initialize(process loopback PCM16 48000 stereo)",
            e,
        )
    })?;
    let declared = DeviceFormat::from_layout(FormatDescription {
        sample_rate: 48_000,
        channels: 2,
        bits_per_sample: 16,
        block_align: 4,
        valid_bits: 16,
        is_float: Some(false),
        extensible: false,
    })?;
    let selected = source.clone();
    let source_check = Box::new(move |before_start| {
        root.alive()?;
        if before_start {
            compare_identity(identity, (identity.0, root.creation()?))?;
            compare_identity(
                identity,
                crate::application_sessions::capture_identity(&selected)?,
            )?;
            root.alive()?;
        }
        Ok(())
    });
    checkpoint()?;
    adopt_initialized_capture(event, client, declared, source_check)
}
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_FAIL;
    #[test]
    fn both_activation_hresult_layers_remain_errors_with_their_operations() {
        let outer = activation_layers(Err(windows::core::Error::from_hresult(E_FAIL)), HRESULT(0))
            .unwrap_err();
        assert!(outer.message.contains("GetActivateResult"));
        let inner = activation_layers(Ok(()), E_ACCESSDENIED).unwrap_err();
        assert_eq!(inner.code, ErrorCode::PermissionDenied);
        assert!(inner.message.contains("activation result"));
        assert!(activation_layers(Ok(()), HRESULT(0)).is_ok());
    }
    #[test]
    fn delayed_callback_after_cancellation_is_drained_and_prevents_start() {
        let signal = Arc::new(CompletionSignal::default());
        let callback: IActivateAudioInterfaceCompletionHandler = Completion {
            signal: signal.clone(),
        }
        .into();
        assert!(
            callback
                .cast::<windows::Win32::System::Com::IAgileObject>()
                .is_ok()
        );
        // Trigger the actual signal path via a separately joined callback simulator.
        let other = signal.clone();
        let notifier = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let callback: IActivateAudioInterfaceCompletionHandler =
                Completion { signal: other }.into();
            // SAFETY: the test invokes the generated COM entry point, which only
            // signals completion and deliberately does not inspect its operation.
            unsafe { callback.ActivateCompleted(None::<&IActivateAudioInterfaceAsyncOperation>) }
                .unwrap();
        });
        let result = drain_completion(&signal, &|| {
            Err(error(ErrorCode::InvalidTransition, "cancelled"))
        });
        notifier.join().unwrap();
        assert!(result.is_err());
        assert!(*signal.done.lock().unwrap(), "must drain before returning");
    }
    #[test]
    fn root_exit_is_source_lost_and_unknown_wait_results_are_explicit() {
        assert!(root_wait_result(WAIT_TIMEOUT.0).is_ok());
        assert_eq!(
            root_wait_result(WAIT_OBJECT_0.0).unwrap_err().code,
            ErrorCode::SourceLost
        );
        assert_eq!(
            root_wait_result(u32::MAX).unwrap_err().code,
            ErrorCode::Internal
        );
    }
}
