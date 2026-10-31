//! Process boundary for an MF transform. The capture process never lends a COM or GPU
//! object pointer to the child: it sends bounded packed BGRA/NV12 copies, or an opt-in
//! duplicated NT texture handle. The child owns its adapter device, encoder-local
//! textures, transform and all pending native input. A failed close or
//! stalled native call can therefore be ended by terminating only this child.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use frame_contracts::RecorderError;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device, ID3D11Device1,
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAS_STILL_DRAWING, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
    IDXGIKeyedMutex, IDXGIResource1,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{CREATE_NO_WINDOW, GetCurrentProcess};
use windows::core::{Interface, PCWSTR};

use super::driver::{
    NativeCollect, NativeEos, NativePacket, NativeStats, NativeSubmit, NvencDriver,
};
use super::mf_transfer::{
    self, HelperTransferStats, MfSharedFallback, MfTransferFormat, MfTransferStats,
    MfTransferTransport,
};
use super::nv12_convert::{Nv12Converter, Nv12Input};
use super::{MfH264Preparation, NativeAdapterIdentity, NativeEncoderConfig, NativeEncoderSettings};
use crate::surface::EncodeTexture;

const CALL_LIMIT: Duration = Duration::from_secs(10);
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
const MAX_HEADER: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
const MAX_REPLY: usize = 48 * 1024 * 1024;
const SHARED_MUTEX_LIMIT_MS: u32 = 250;
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Deserialize)]
enum Request {
    Open {
        adapter: NativeAdapterIdentity,
        config: NativeEncoderConfig,
        format: MfTransferFormat,
        generation: u64,
        #[serde(default)]
        codec: u32,
    },
    Register {
        slot: u32,
    },
    RegisterShared {
        slot: u32,
        generation: u64,
        adapter_luid: u64,
        /// A handle value in the helper's handle table, created by DuplicateHandle.
        handle: u64,
    },
    Unregister {
        slot: u32,
    },
    Submit {
        slot: u32,
        pts: u64,
        duration: u64,
        idr: bool,
        bytes: u32,
    },
    SubmitShared {
        slot: u32,
        generation: u64,
        sequence: u64,
        pts: u64,
        duration: u64,
        idr: bool,
    },
    Collect {
        slot: u32,
        wait_ms: u32,
        capacity: usize,
    },
    BeginEos,
    PollEos {
        wait_ms: u32,
    },
    Close,
    Qualify {
        adapter: NativeAdapterIdentity,
        config: NativeEncoderConfig,
        #[serde(default)]
        codec: u32,
    },
    Prepare {
        adapter: NativeAdapterIdentity,
        config: NativeEncoderConfig,
    },
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Open(NativeEncoderSettings),
    SessionOpened {
        settings: NativeEncoderSettings,
        format: MfTransferFormat,
    },
    Preparations(Vec<MfH264Preparation>),
    Registered(u32),
    SharedUnavailable(RecorderError),
    Done,
    Submit {
        slot: u32,
        buffered: bool,
    },
    Busy,
    Pending,
    Packet(NativePacket),
    Bool(bool),
    Error(RecorderError),
    Closed {
        error: Option<RecorderError>,
        unsafe_native: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct Response {
    reply: Reply,
    stats: Option<NativeStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transfer: Option<HelperTransferStats>,
}

fn write_reply<W: Write>(
    output: &mut W,
    reply: Reply,
    stats: Option<NativeStats>,
) -> Result<(), RecorderError> {
    write_session_reply(output, reply, stats, None)
}

fn write_session_reply<W: Write>(
    output: &mut W,
    reply: Reply,
    stats: Option<NativeStats>,
    transfer: Option<HelperTransferStats>,
) -> Result<(), RecorderError> {
    write_message(
        output,
        &Response {
            reply,
            stats,
            transfer,
        },
    )
}

fn close_result(reply: Reply, status: std::process::ExitStatus) -> Result<(), RecorderError> {
    match reply {
        Reply::Closed {
            error: Some(error), ..
        } => Err(error),
        Reply::Closed {
            unsafe_native: true,
            ..
        } => Err(io_error("close", "native shutdown was not confirmed")),
        Reply::Closed {
            unsafe_native: false,
            ..
        } if status.success() => Ok(()),
        Reply::Closed { .. } => Err(io_error("close", "helper exited unsuccessfully")),
        _ => Err(io_error("close", "unexpected helper reply")),
    }
}

fn io_error(operation: &str, error: impl std::fmt::Display) -> RecorderError {
    super::internal_error(format!(
        "MF helper {operation}: {error}. The isolated session was stopped; discard this stream."
    ))
}

fn write_message<W: Write>(output: &mut W, value: &impl Serialize) -> Result<(), RecorderError> {
    let bytes = serde_json::to_vec(value).map_err(|e| io_error("serialization", e))?;
    if bytes.len() > MAX_REPLY {
        return Err(io_error("serialization", "message exceeds the IPC bound"));
    }
    output
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(|e| io_error("write", e))?;
    output.write_all(&bytes).map_err(|e| io_error("write", e))?;
    output.flush().map_err(|e| io_error("flush", e))
}

fn read_message<R: Read, T: for<'de> Deserialize<'de>>(
    input: &mut R,
    max: usize,
) -> Result<T, RecorderError> {
    let mut size = [0u8; 4];
    input
        .read_exact(&mut size)
        .map_err(|e| io_error("read", e))?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > max {
        return Err(io_error("read", "invalid or oversized IPC message"));
    }
    let mut bytes = vec![0u8; size];
    input
        .read_exact(&mut bytes)
        .map_err(|e| io_error("read", e))?;
    serde_json::from_slice(&bytes).map_err(|e| io_error("decode", e))
}

struct Channel {
    _job: Job,
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    stdout: ChildStdout,
    deadline: Arc<Mutex<Option<Instant>>>,
    stop_watch: Arc<AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
    last_stats: Option<NativeStats>,
    last_transfer: Option<HelperTransferStats>,
}

/// Closing this parent's job handle terminates any still-running MF child. This
/// also covers an unexpected parent exit that cannot execute Channel::drop.
struct Job(windows::Win32::Foundation::HANDLE);

impl Job {
    fn assign(child: &mut Child) -> Result<Self, RecorderError> {
        let raw =
            unsafe { CreateJobObjectW(None, None) }.map_err(|e| io_error("job creation", e))?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if let Err(error) = configured {
            unsafe {
                let _ = CloseHandle(raw);
            }
            return Err(io_error("job configuration", error));
        }
        let process = windows::Win32::Foundation::HANDLE(child.as_raw_handle());
        if let Err(error) = unsafe { AssignProcessToJobObject(raw, process) } {
            unsafe {
                let _ = CloseHandle(raw);
            }
            return Err(io_error("job assignment", error));
        }
        Ok(Self(raw))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Channel {
    fn duplicate_shared_handle(&self, source: HANDLE) -> Result<u64, RecorderError> {
        let child = self.child.lock().map_err(|e| io_error("process lock", e))?;
        let mut remote = HANDLE::default();
        // The handle is unnamed and non-inheritable; only this owned child receives it.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source,
                HANDLE(child.as_raw_handle()),
                &mut remote,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .map_err(|e| io_error("shared handle duplication", e))?;
        Ok(remote.0 as usize as u64)
    }
    fn start() -> Result<Self, RecorderError> {
        let exe = std::env::current_exe().map_err(|e| io_error("executable lookup", e))?;
        let child = Command::new(exe)
            .arg("--frame-mf-helper")
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| io_error("spawn", e))?;
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> Result<Self, RecorderError> {
        let job = match Job::assign(&mut child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let until = Instant::now() + CLOSE_LIMIT;
                while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(error);
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io_error("spawn", "no input pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io_error("spawn", "no output pipe"))?;
        let child = Arc::new(Mutex::new(child));
        let deadline = Arc::new(Mutex::new(None::<Instant>));
        let stop_watch = Arc::new(AtomicBool::new(false));
        let watch_child = Arc::clone(&child);
        let watch_deadline = Arc::clone(&deadline);
        let watch_stop = Arc::clone(&stop_watch);
        let watcher = std::thread::Builder::new()
            .name("frame-mf-watchdog".into())
            .spawn(move || {
                while !watch_stop.load(Ordering::Acquire) {
                    let expired = watch_deadline
                        .lock()
                        .ok()
                        .and_then(|d| *d)
                        .is_some_and(|d| Instant::now() >= d);
                    if expired {
                        if let Ok(mut child) = watch_child.lock() {
                            let _ = child.kill();
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .map_err(|e| {
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                }
                io_error("watchdog spawn", e)
            })?;
        Ok(Self {
            _job: job,
            child,
            stdin,
            stdout,
            deadline,
            stop_watch,
            watcher: Some(watcher),
            last_stats: None,
            last_transfer: None,
        })
    }

    fn exchange(
        &mut self,
        request: Request,
        payload: &[u8],
        limit: Duration,
    ) -> Result<Reply, RecorderError> {
        *self
            .deadline
            .lock()
            .map_err(|e| io_error("deadline lock", e))? = Some(Instant::now() + limit);
        let result = (|| {
            let header = serde_json::to_vec(&request).map_err(|e| io_error("request", e))?;
            if header.is_empty() || header.len() > MAX_HEADER || payload.len() > MAX_FRAME_BYTES {
                return Err(io_error("request", "IPC bound exceeded"));
            }
            self.stdin
                .write_all(&(header.len() as u32).to_le_bytes())
                .map_err(|e| io_error("write", e))?;
            self.stdin
                .write_all(&header)
                .map_err(|e| io_error("write", e))?;
            self.stdin
                .write_all(payload)
                .map_err(|e| io_error("frame write", e))?;
            self.stdin.flush().map_err(|e| io_error("flush", e))?;
            let response: Response = read_message(&mut self.stdout, MAX_REPLY)?;
            self.last_stats = response.stats;
            self.last_transfer = response.transfer;
            Ok(response.reply)
        })();
        if let Ok(mut deadline) = self.deadline.lock() {
            *deadline = None;
        }
        result
    }

    fn terminate_and_reap(&mut self) -> Result<(), RecorderError> {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
        let until = Instant::now() + CLOSE_LIMIT;
        loop {
            let status = self
                .child
                .lock()
                .map_err(|e| io_error("process lock", e))?
                .try_wait();
            match status {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
                Ok(None) => return Err(io_error("termination", "child did not exit")),
                Err(e) => return Err(io_error("wait", e)),
            }
        }
    }

    fn reap_after_close(&mut self) -> Result<std::process::ExitStatus, RecorderError> {
        let until = Instant::now() + CLOSE_LIMIT;
        loop {
            let status = self
                .child
                .lock()
                .map_err(|e| io_error("process lock", e))?
                .try_wait();
            match status {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    self.terminate_and_reap()?;
                    return Err(io_error("close", "helper did not exit after close"));
                }
                Err(e) => return Err(io_error("wait", e)),
            }
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
        self.stop_watch.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

struct ParentRegistration {
    source: ID3D11Texture2D,
    staging: Option<ID3D11Texture2D>,
    conversion: Option<Nv12Input>,
    shared: Option<ParentShared>,
}

struct OwnedNtHandle(HANDLE);

impl Drop for OwnedNtHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct ParentShared {
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    _handle: OwnedNtHandle,
    sequence: u64,
}

struct HelperShared {
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    sequence: u64,
}

/// WAIT_TIMEOUT and WAIT_ABANDONED are positive HRESULTs. Do not use the
/// generated Result wrapper, which would incorrectly treat either as acquired.
fn acquire_shared(mutex: &IDXGIKeyedMutex, key: u64) -> Result<(), RecorderError> {
    let result = unsafe {
        (Interface::vtable(mutex).AcquireSync)(mutex.as_raw(), key, SHARED_MUTEX_LIMIT_MS)
    };
    classify_shared_acquire(result.0 as u32)
}

fn classify_shared_acquire(status: u32) -> Result<(), RecorderError> {
    match status {
        0 => Ok(()),
        0x102 => Err(io_error("shared mutex", "250 ms ownership timeout")),
        0x80 => Err(io_error(
            "shared mutex",
            "abandoned resource; restart the session",
        )),
        _ => Err(io_error("shared mutex", format!("HRESULT 0x{status:08X}"))),
    }
}

fn create_parent_shared(
    device: &ID3D11Device,
    source: &ID3D11Texture2D,
) -> Result<ParentShared, RecorderError> {
    // Require the NT-handle APIs before creating a resource the child cannot import.
    let _device1: ID3D11Device1 = device.cast().map_err(|e| io_error("ID3D11Device1", e))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        source.GetDesc(&mut desc);
    }
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
    desc.CPUAccessFlags = 0;
    desc.MiscFlags =
        (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32;
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| io_error("shared texture allocation", e))?;
    let texture = texture.ok_or_else(|| io_error("shared texture allocation", "no texture"))?;
    let mutex = texture
        .cast()
        .map_err(|e| io_error("shared mutex interface", e))?;
    let resource: IDXGIResource1 = texture.cast().map_err(|e| io_error("IDXGIResource1", e))?;
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )
    }
    .map_err(|e| io_error("shared NT handle", e))?;
    Ok(ParentShared {
        texture,
        mutex,
        _handle: OwnedNtHandle(handle),
        sequence: 0,
    })
}

fn validate_shared_sequence(
    generation: u64,
    expected_generation: u64,
    sequence: u64,
    previous: u64,
) -> Result<(), RecorderError> {
    if generation == 0
        || generation != expected_generation
        || previous.checked_add(1) != Some(sequence)
    {
        return Err(io_error(
            "shared submit",
            "stale generation or out-of-order sequence",
        ));
    }
    Ok(())
}

fn import_helper_shared(
    device: &ID3D11Device,
    raw_handle: u64,
    width: u32,
    height: u32,
) -> Result<HelperShared, RecorderError> {
    let raw = usize::try_from(raw_handle)
        .map_err(|_| io_error("shared import", "handle exceeds pointer width"))?;
    if raw == 0 || raw == usize::MAX {
        return Err(io_error("shared import", "invalid NT handle"));
    }
    let handle = OwnedNtHandle(HANDLE(raw as *mut c_void));
    let device1: ID3D11Device1 = device.cast().map_err(|e| io_error("ID3D11Device1", e))?;
    let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(handle.0) }
        .map_err(|e| io_error("OpenSharedResource1", e))?;
    // Open owns a COM resource reference; the duplicated handle can close now.
    drop(handle);
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }
    let required =
        (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32;
    if desc.Width != width
        || desc.Height != height
        || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
        || desc.SampleDesc.Count != 1
        || desc.MipLevels != 1
        || desc.ArraySize != 1
        || desc.CPUAccessFlags != 0
        || desc.Usage != D3D11_USAGE_DEFAULT
        || desc.MiscFlags & required != required
    {
        return Err(io_error(
            "shared import",
            "resource geometry/format/flags mismatch",
        ));
    }
    let mutex = texture
        .cast()
        .map_err(|e| io_error("shared mutex interface", e))?;
    Ok(HelperShared {
        texture,
        mutex,
        sequence: 0,
    })
}

fn helper_input_texture(
    device: &ID3D11Device,
    config: &NativeEncoderConfig,
    format: MfTransferFormat,
) -> Result<ID3D11Texture2D, RecorderError> {
    use windows::Win32::Graphics::Direct3D11::D3D11_BIND_VIDEO_ENCODER;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: config.width,
        Height: config.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: FrameLayout::new(config.width, config.height, format)?.dxgi_format(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: if format == MfTransferFormat::Nv12 {
            D3D11_BIND_VIDEO_ENCODER.0 as u32
        } else {
            0
        },
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| io_error("helper texture allocation", e))?;
    texture.ok_or_else(|| io_error("helper texture allocation", "no texture"))
}

fn helper_register(
    driver: &mut super::media_foundation::MfDriver,
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    config: &NativeEncoderConfig,
) -> Result<u32, RecorderError> {
    driver.register_input(EncodeTexture {
        device: device.as_raw(),
        texture: texture.as_raw(),
        identity: crate::surface::SurfaceIdentity {
            pool: 0,
            device: device.as_raw() as usize,
            texture: texture.as_raw() as usize,
            width: config.width,
            height: config.height,
        },
    })
}

fn submit_reply(result: Result<NativeSubmit, RecorderError>) -> Reply {
    match result {
        Ok(NativeSubmit::Pending { slot }) => Reply::Submit {
            slot,
            buffered: false,
        },
        Ok(NativeSubmit::Buffered { slot }) => Reply::Submit {
            slot,
            buffered: true,
        },
        Ok(NativeSubmit::Busy) => Reply::Busy,
        Err(error) => Reply::Error(error),
    }
}

/// Wire rows are tightly packed. NV12 is Y followed by interleaved UV with the
/// same row width and half as many UV rows; D3D RowPitch padding is removed.
#[derive(Clone, Copy, Debug)]
struct FrameLayout {
    format: MfTransferFormat,
    row_bytes: usize,
    rows: usize,
    bytes: usize,
}

impl FrameLayout {
    fn new(width: u32, height: u32, format: MfTransferFormat) -> Result<Self, RecorderError> {
        if width == 0
            || height == 0
            || (format == MfTransferFormat::Nv12
                && (!width.is_multiple_of(2) || !height.is_multiple_of(2)))
        {
            return Err(io_error(
                "frame layout",
                "invalid geometry for the transfer format",
            ));
        }
        let row_bytes = (width as usize).checked_mul(if format == MfTransferFormat::Bgra8 {
            4
        } else {
            1
        });
        let rows = (height as usize).checked_add(if format == MfTransferFormat::Nv12 {
            height as usize / 2
        } else {
            0
        });
        let (row_bytes, rows) = row_bytes
            .zip(rows)
            .ok_or_else(|| io_error("frame layout", "row overflow"))?;
        let bytes = row_bytes
            .checked_mul(rows)
            .filter(|n| *n <= MAX_FRAME_BYTES)
            .ok_or_else(|| io_error("frame layout", "frame exceeds the IPC bound"))?;
        Ok(Self {
            format,
            row_bytes,
            rows,
            bytes,
        })
    }

    fn dxgi_format(self) -> DXGI_FORMAT {
        match self.format {
            MfTransferFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            MfTransferFormat::Nv12 => DXGI_FORMAT_NV12,
        }
    }
}

fn allocate_frame_buffer(size: usize) -> Result<Vec<u8>, RecorderError> {
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(io_error("frame allocation", "frame exceeds the IPC bound"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|e| io_error("frame allocation", e))?;
    bytes.resize(size, 0);
    Ok(bytes)
}

/// The caller owns a live D3D mapping with `height` rows of `RowPitch` bytes.
/// Source and destination must not alias. Every valid pixel byte is overwritten.
unsafe fn copy_frame_rows(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    row_bytes: usize,
    height: usize,
    destination: &mut [u8],
) -> Result<(), RecorderError> {
    if mapped.pData.is_null()
        || (mapped.RowPitch as usize) < row_bytes
        || row_bytes.checked_mul(height) != Some(destination.len())
        || (mapped.RowPitch as usize)
            .checked_mul(height)
            .is_none_or(|n| n > isize::MAX as usize)
    {
        return Err(io_error(
            "staging map",
            "invalid row pitch or destination size",
        ));
    }
    for y in 0..height {
        // SAFETY: the caller retains the mapping; the checks above validate the
        // destination and row width, and the D3D texture supplies all source rows.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                destination.as_mut_ptr().add(y * row_bytes),
                row_bytes,
            );
        }
    }
    Ok(())
}

pub(super) struct MfProcessDriver {
    channel: Channel,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    registrations: Vec<Option<ParentRegistration>>,
    settings: NativeEncoderSettings,
    layout: FrameLayout,
    converter: Option<Nv12Converter>,
    frame_buffer: Vec<u8>,
    transfer: Option<MfTransferStats>,
    requested_transport: MfTransferTransport,
    generation: u64,
    adapter_luid: u64,
    config: NativeEncoderConfig,
    fallback_reason: Option<String>,
    closed: bool,
}

// All D3D calls remain on PacketEncoder's single worker; transfer only moves the
// newly built owner to that worker.
unsafe impl Send for MfProcessDriver {}

impl MfProcessDriver {
    pub(super) fn open(
        raw_device: *mut c_void,
        adapter: &NativeAdapterIdentity,
        config: &NativeEncoderConfig,
    ) -> Result<Self, RecorderError> {
        Self::open_profile(raw_device, adapter, config, 0)
    }
    pub(super) fn open_profile(
        raw_device: *mut c_void,
        adapter: &NativeAdapterIdentity,
        config: &NativeEncoderConfig,
        codec: u32,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        let format = mf_transfer::selected_format()?;
        let requested_transport = mf_transfer::selected_transport()?;
        let generation = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        if generation == 0 {
            return Err(io_error("open", "session generation exhausted"));
        }
        let layout = FrameLayout::new(config.width, config.height, format)?;
        let borrowed = unsafe { ID3D11Device::from_raw_borrowed(&raw_device) }
            .ok_or_else(|| io_error("open", "null capture device"))?;
        let device = borrowed.clone();
        let actual = super::native::device_adapter(raw_device)?;
        if actual.luid != adapter.luid {
            return Err(io_error("open", "capture adapter LUID changed"));
        }
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|e| io_error("D3D context", e))?;
        let converter = if format == MfTransferFormat::Nv12
            && requested_transport == MfTransferTransport::Cpu
        {
            Some(Nv12Converter::new(&device, config)?)
        } else {
            None
        };
        let mut channel = Channel::start()?;
        let settings = match channel.exchange(
            Request::Open {
                adapter: adapter.clone(),
                config: *config,
                format,
                generation,
                codec,
            },
            &[],
            CALL_LIMIT,
        )? {
            Reply::SessionOpened {
                settings,
                format: received,
            } if received == format => settings,
            Reply::Error(error) => return Err(error),
            _ => return Err(io_error("open", "unexpected helper reply")),
        };
        if channel.last_stats.is_none() {
            return Err(io_error("open", "helper omitted native counters"));
        }
        if settings.width != config.width || settings.height != config.height {
            return Err(io_error("open", "helper changed the transfer geometry"));
        }
        let frame_buffer = if requested_transport == MfTransferTransport::Cpu {
            allocate_frame_buffer(layout.bytes)?
        } else {
            Vec::new()
        };
        let transfer = mf_transfer::enabled().then(|| MfTransferStats {
            format,
            requested_transport,
            packed_frame_bytes: layout.bytes as u64,
            parent_buffer_allocations: u64::from(!frame_buffer.is_empty()),
            parent_buffer_capacity: frame_buffer.capacity() as u64,
            ..MfTransferStats::default()
        });
        Ok(Self {
            channel,
            device,
            context,
            registrations: Vec::new(),
            settings,
            layout,
            converter,
            frame_buffer,
            transfer,
            requested_transport,
            generation,
            adapter_luid: adapter.luid,
            config: *config,
            fallback_reason: None,
            closed: false,
        })
    }

    fn read_frame_bytes(&mut self, registration: u32) -> Result<(), RecorderError> {
        let item = self
            .registrations
            .get(registration as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| io_error("submit", "unknown registration"))?;
        if self.frame_buffer.len() != self.layout.bytes {
            return Err(io_error("submit", "frame buffer geometry changed"));
        }
        if let Some(input) = &item.conversion {
            let started = self.transfer.as_ref().map(|_| Instant::now());
            self.converter
                .as_ref()
                .ok_or_else(|| io_error("conversion", "converter unavailable"))?
                .convert(input)?;
            if let Some(transfer) = self.transfer.as_mut() {
                transfer
                    .conversion
                    .record(started.expect("enabled timing").elapsed());
            }
        }
        let started = self.transfer.as_ref().map(|_| Instant::now());
        let dest: ID3D11Resource = item
            .staging
            .as_ref()
            .ok_or_else(|| io_error("readback", "GPU registration cannot be mapped"))?
            .cast()
            .map_err(|e| io_error("staging cast", e))?;
        let source: ID3D11Resource = item.source.cast().map_err(|e| io_error("source cast", e))?;
        unsafe {
            self.context.CopyResource(&dest, &source);
            self.context.Flush();
        }
        let until = Instant::now() + CALL_LIMIT;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        loop {
            match unsafe {
                self.context.Map(
                    &dest,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => break,
                Err(error)
                    if error.code() == DXGI_ERROR_WAS_STILL_DRAWING && Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => return Err(io_error("staging map", error)),
            }
        }
        // SAFETY: Map exposes BGRA rows or NV12's contiguous Y + UV rows.
        // Unmap follows every copy, including when the pitch check fails.
        let outcome = unsafe {
            copy_frame_rows(
                &mapped,
                self.layout.row_bytes,
                self.layout.rows,
                &mut self.frame_buffer,
            )
        };
        unsafe {
            self.context.Unmap(&dest, 0);
        }
        if let Some(transfer) = self.transfer.as_mut() {
            transfer
                .readback
                .record(started.expect("enabled timing").elapsed());
        }
        outcome
    }

    fn answer(&mut self, request: Request, payload: &[u8]) -> Result<Reply, RecorderError> {
        match self.channel.exchange(request, payload, CALL_LIMIT)? {
            Reply::Error(error) => Err(error),
            other => Ok(other),
        }
    }
}

impl NvencDriver for MfProcessDriver {
    fn settings(&self) -> &NativeEncoderSettings {
        &self.settings
    }

    fn register_input(&mut self, texture: EncodeTexture) -> Result<u32, RecorderError> {
        if texture.device != self.device.as_raw() || self.registrations.len() >= 4 {
            return Err(io_error(
                "register",
                "wrong device or more than four textures",
            ));
        }
        let source = unsafe { ID3D11Texture2D::from_raw_borrowed(&texture.texture) }
            .ok_or_else(|| io_error("register", "null texture"))?
            .clone();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            source.GetDesc(&mut desc);
        }
        if desc.Width != self.settings.width
            || desc.Height != self.settings.height
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || desc.SampleDesc.Count != 1
            || desc.MipLevels != 1
            || desc.ArraySize != 1
        {
            return Err(io_error("register", "texture format or geometry mismatch"));
        }
        let slot = self.registrations.len() as u32;
        if self.requested_transport == MfTransferTransport::SharedGpu {
            let prepared = create_parent_shared(&self.device, &source).and_then(|shared| {
                let handle = self.channel.duplicate_shared_handle(shared._handle.0)?;
                Ok((shared, handle))
            });
            let fallback = match prepared {
                Ok((shared, handle)) => {
                    // The child closes this duplicated handle on every import outcome.
                    // If IPC fails, terminate it so an undelivered handle cannot leak.
                    let response = self.channel.exchange(
                        Request::RegisterShared {
                            slot,
                            generation: self.generation,
                            adapter_luid: self.adapter_luid,
                            handle,
                        },
                        &[],
                        CALL_LIMIT,
                    );
                    match response {
                        Ok(Reply::Registered(remote)) if remote == slot => {
                            self.registrations.push(Some(ParentRegistration {
                                source,
                                staging: None,
                                conversion: None,
                                shared: Some(shared),
                            }));
                            if let Some(stats) = &mut self.transfer {
                                stats.shared_gpu_registrations += 1;
                            }
                            return Ok(slot);
                        }
                        Ok(Reply::SharedUnavailable(error)) => {
                            (MfSharedFallback::HelperSharedResourceUnavailable, error)
                        }
                        Ok(Reply::Error(error)) => return Err(error),
                        Ok(_) => {
                            return Err(io_error("shared register", "unexpected helper reply"));
                        }
                        Err(error) => {
                            let _ = self.channel.terminate_and_reap();
                            return Err(error);
                        }
                    }
                }
                Err(error) => (MfSharedFallback::ParentSharedResourceUnavailable, error),
            };
            self.fallback_reason = Some(fallback.1.message);
            if let Some(stats) = &mut self.transfer {
                stats.last_shared_fallback = Some(fallback.0);
            }
        }
        // A registration falls back before any frame was submitted. Never change
        // transport after ownership/protocol failure in an active GPU registration.
        if self.frame_buffer.is_empty() {
            self.frame_buffer = allocate_frame_buffer(self.layout.bytes)?;
            if let Some(stats) = &mut self.transfer {
                stats.parent_buffer_allocations += 1;
                stats.parent_buffer_capacity = self.frame_buffer.capacity() as u64;
            }
        }
        if self.layout.format == MfTransferFormat::Nv12 && self.converter.is_none() {
            self.converter = Some(Nv12Converter::new(&self.device, &self.config)?);
        }
        let conversion = self
            .converter
            .as_ref()
            .map(|converter| converter.prepare(&source))
            .transpose()?;
        let source = conversion
            .as_ref()
            .map_or(source, |input| input.nv12.clone());
        unsafe { source.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .map_err(|e| io_error("staging allocation", e))?;
        let staging = staging.ok_or_else(|| io_error("staging allocation", "no texture"))?;
        match self.answer(Request::Register { slot }, &[])? {
            Reply::Registered(remote) if remote == slot => {
                self.registrations.push(Some(ParentRegistration {
                    source,
                    staging: Some(staging),
                    conversion,
                    shared: None,
                }));
                if let Some(stats) = &mut self.transfer {
                    stats.cpu_registrations += 1;
                }
                Ok(slot)
            }
            _ => Err(io_error("register", "unexpected helper reply")),
        }
    }

    fn unregister_input(&mut self, registration: u32) -> Result<(), RecorderError> {
        match self.answer(Request::Unregister { slot: registration }, &[])? {
            Reply::Done => {
                self.registrations
                    .get_mut(registration as usize)
                    .ok_or_else(|| io_error("unregister", "unknown slot"))?
                    .take();
                Ok(())
            }
            _ => Err(io_error("unregister", "unexpected helper reply")),
        }
    }

    fn submit(
        &mut self,
        registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        force_idr: bool,
    ) -> Result<NativeSubmit, RecorderError> {
        let shared = self
            .registrations
            .get_mut(registration as usize)
            .and_then(Option::as_mut)
            .ok_or_else(|| io_error("submit", "unknown registration"))?;
        if let Some(gpu) = &mut shared.shared {
            let sequence = gpu
                .sequence
                .checked_add(1)
                .ok_or_else(|| io_error("shared submit", "sequence exhausted"))?;
            let started = self.transfer.as_ref().map(|_| Instant::now());
            acquire_shared(&gpu.mutex, 0)?;
            unsafe {
                self.context.CopyResource(&gpu.texture, &shared.source);
                self.context.Flush();
            }
            unsafe { gpu.mutex.ReleaseSync(1) }
                .map_err(|e| io_error("shared producer release", e))?;
            gpu.sequence = sequence;
            if let Some(stats) = &mut self.transfer {
                stats.submit_attempts += 1;
                stats.shared_gpu_attempts += 1;
                stats
                    .shared_gpu_copy
                    .record(started.expect("enabled timing").elapsed());
            }
            let started = self.transfer.as_ref().map(|_| Instant::now());
            let reply = self.channel.exchange(
                Request::SubmitShared {
                    slot: registration,
                    generation: self.generation,
                    sequence,
                    pts: pts_ns,
                    duration: duration_ns,
                    idr: force_idr,
                },
                &[],
                CALL_LIMIT,
            );
            if let Some(stats) = &mut self.transfer {
                stats
                    .ipc_roundtrip
                    .record(started.expect("enabled timing").elapsed());
                if let Some(helper) = self.channel.last_transfer {
                    stats.observe_helper(helper);
                }
            }
            return match reply? {
                Reply::Submit {
                    slot,
                    buffered: false,
                } => Ok(NativeSubmit::Pending { slot }),
                Reply::Submit {
                    slot,
                    buffered: true,
                } => Ok(NativeSubmit::Buffered { slot }),
                Reply::Busy => Ok(NativeSubmit::Busy),
                Reply::Error(error) => Err(error),
                _ => Err(io_error("shared submit", "unexpected helper reply")),
            };
        }
        self.read_frame_bytes(registration)?;
        if let Some(transfer) = self.transfer.as_mut() {
            transfer.submit_attempts = transfer.submit_attempts.saturating_add(1);
            transfer.payload_bytes = transfer
                .payload_bytes
                .saturating_add(self.frame_buffer.len() as u64);
        }
        let request = Request::Submit {
            slot: registration,
            pts: pts_ns,
            duration: duration_ns,
            idr: force_idr,
            bytes: self.frame_buffer.len() as u32,
        };
        let started = self.transfer.as_ref().map(|_| Instant::now());
        // The blocking exchange consumes the borrowed bytes before it returns.
        // Neither the pipe nor the child owns this parent buffer afterward.
        let reply = self
            .channel
            .exchange(request, &self.frame_buffer, CALL_LIMIT);
        if let Some(transfer) = self.transfer.as_mut() {
            transfer
                .ipc_roundtrip
                .record(started.expect("enabled timing").elapsed());
            if let Some(helper) = self.channel.last_transfer {
                transfer.observe_helper(helper);
            }
        }
        match reply? {
            Reply::Error(error) => Err(error),
            Reply::Submit {
                slot,
                buffered: false,
            } => Ok(NativeSubmit::Pending { slot }),
            Reply::Submit {
                slot,
                buffered: true,
            } => Ok(NativeSubmit::Buffered { slot }),
            Reply::Busy => Ok(NativeSubmit::Busy),
            _ => Err(io_error("submit", "unexpected helper reply")),
        }
    }

    fn collect(
        &mut self,
        slot: u32,
        wait_ms: u32,
        capacity: usize,
    ) -> Result<NativeCollect, RecorderError> {
        match self.answer(
            Request::Collect {
                slot,
                wait_ms,
                capacity,
            },
            &[],
        )? {
            Reply::Packet(packet) if packet.data.len() <= capacity => {
                Ok(NativeCollect::Ready(packet))
            }
            Reply::Pending => Ok(NativeCollect::Pending),
            _ => Err(io_error("collect", "unexpected or oversized helper packet")),
        }
    }
    fn begin_eos(&mut self) -> Result<NativeEos, RecorderError> {
        match self.answer(Request::BeginEos, &[])? {
            Reply::Done => Ok(NativeEos::Submitted),
            Reply::Busy => Ok(NativeEos::Busy),
            _ => Err(io_error("EOS", "unexpected helper reply")),
        }
    }
    fn poll_eos(&mut self, wait_ms: u32) -> Result<bool, RecorderError> {
        match self.answer(Request::PollEos { wait_ms }, &[])? {
            Reply::Bool(done) => Ok(done),
            _ => Err(io_error("drain", "unexpected helper reply")),
        }
    }
    fn stats(&self) -> NativeStats {
        self.channel
            .last_stats
            .expect("open requires native counters")
    }
    fn mf_transfer_stats(&self) -> Option<MfTransferStats> {
        self.transfer
    }
    fn phase(&self) -> String {
        let gpu = self
            .registrations
            .iter()
            .flatten()
            .filter(|r| r.shared.is_some())
            .count();
        let cpu = self
            .registrations
            .iter()
            .flatten()
            .filter(|r| r.shared.is_none())
            .count();
        format!(
            "isolated MF helper: shared_gpu={gpu}, cpu={cpu}; fallback={}",
            self.fallback_reason.as_deref().unwrap_or("none")
        )
    }
    fn close(&mut self) -> Result<(), RecorderError> {
        if self.closed {
            return Ok(());
        }
        let reply = self.channel.exchange(Request::Close, &[], CLOSE_LIMIT);
        let result = match reply {
            Ok(reply @ Reply::Closed { .. }) => self
                .channel
                .reap_after_close()
                .and_then(|status| close_result(reply, status)),
            Ok(_) => self
                .channel
                .terminate_and_reap()
                .and(Err(io_error("close", "unexpected helper reply"))),
            Err(error) => self.channel.terminate_and_reap().and(Err(error)),
        };
        // COM pointers never cross this boundary. Shared resources have separate
        // device owners and no native pending input references the parent resource.
        // After the bounded termination attempt, registrations can be released even if Windows
        // has not yet reported the child's exit; its job still owns that process.
        self.registrations.clear();
        self.frame_buffer = Vec::new();
        self.closed = true;
        result
    }
}

pub(super) fn qualify_adapter(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
) -> Result<NativeEncoderSettings, RecorderError> {
    qualify_profile(adapter, config, 0)
}
pub(super) fn qualify_profile(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    codec: u32,
) -> Result<NativeEncoderSettings, RecorderError> {
    let mut channel = Channel::start()?;
    let result = match channel.exchange(
        Request::Qualify {
            adapter: adapter.clone(),
            config: *config,
            codec,
        },
        &[],
        CALL_LIMIT,
    )? {
        Reply::Open(settings) => Ok(settings),
        Reply::Error(error) => Err(error),
        _ => Err(io_error("qualification", "unexpected helper reply")),
    };
    if !channel.reap_after_close()?.success() {
        return Err(io_error("qualification", "helper exited unsuccessfully"));
    }
    result
}

pub fn prepare_hardware_h264(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
) -> Result<Vec<MfH264Preparation>, RecorderError> {
    let mut channel = Channel::start()?;
    let result = match channel.exchange(
        Request::Prepare {
            adapter: adapter.clone(),
            config: *config,
        },
        &[],
        CALL_LIMIT,
    )? {
        Reply::Preparations(values) => Ok(values),
        Reply::Error(error) => Err(error),
        _ => Err(io_error("preparation", "unexpected helper reply")),
    };
    if !channel.reap_after_close()?.success() {
        return Err(io_error("preparation", "helper exited unsuccessfully"));
    }
    result
}

/// Entry point called by the desktop executable before any UI or recording state exists.
pub(crate) fn run_helper() -> i32 {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let first: Request = match read_message(&mut input, MAX_HEADER) {
        Ok(value) => value,
        Err(_) => return 2,
    };
    let (adapter, config, format, generation, codec) = match first {
        Request::Open {
            adapter,
            config,
            format,
            generation,
            codec,
        } if generation != 0 => (adapter, config, format, generation, codec),
        Request::Qualify {
            adapter,
            config,
            codec,
        } => {
            let reply = match super::media_foundation::qualify_profile(&adapter, &config, codec) {
                Ok(settings) => Reply::Open(settings),
                Err(error) => Reply::Error(error),
            };
            let _ = write_reply(&mut output, reply, None);
            return 0;
        }
        Request::Prepare { adapter, config } => {
            let reply = match super::media_foundation::prepare_hardware_h264(&adapter, &config) {
                Ok(values) => Reply::Preparations(values),
                Err(error) => Reply::Error(error),
            };
            let _ = write_reply(&mut output, reply, None);
            return 0;
        }
        _ => return 2,
    };
    let layout = match config
        .validate()
        .and_then(|_| FrameLayout::new(config.width, config.height, format))
    {
        Ok(layout) => layout,
        Err(error) => {
            let _ = write_reply(&mut output, Reply::Error(error), None);
            return 2;
        }
    };
    let mut pixels = Vec::new();
    let mut transfer = mf_transfer::enabled().then(HelperTransferStats::default);
    let device = match super::native::qualification_device_for_luid(adapter.luid) {
        Ok(value) => value,
        Err(error) => {
            let _ = write_reply(&mut output, Reply::Error(error), None);
            return 2;
        }
    };
    let mut driver = match super::media_foundation::MfDriver::open_profile(
        device.as_raw(),
        &adapter,
        &config,
        codec,
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = write_reply(&mut output, Reply::Error(error), None);
            return 2;
        }
    };
    if write_reply(
        &mut output,
        Reply::SessionOpened {
            settings: driver.settings().clone(),
            format,
        },
        Some(driver.stats()),
    )
    .is_err()
    {
        return 2;
    }
    let context = match unsafe { device.GetImmediateContext() } {
        Ok(value) => value,
        Err(_) => return 2,
    };
    let mut textures: Vec<ID3D11Texture2D> = Vec::new();
    let mut shared_textures: Vec<Option<HelperShared>> = Vec::new();
    loop {
        let request: Request = match read_message(&mut input, MAX_HEADER) {
            Ok(value) => value,
            Err(_) => return 2,
        };
        let reply = match request {
            Request::Register { slot } => {
                if slot != textures.len() as u32 || textures.len() >= 4 {
                    Reply::Error(io_error("register", "invalid slot"))
                } else {
                    let prepared = (|| {
                        if pixels.is_empty() {
                            pixels = allocate_frame_buffer(layout.bytes)?;
                            if let Some(stats) = &mut transfer {
                                stats.buffer_allocations += 1;
                                stats.buffer_capacity = pixels.capacity() as u64;
                            }
                        }
                        let texture = helper_input_texture(&device, &config, format)?;
                        let remote = helper_register(&mut driver, &device, &texture, &config)?;
                        Ok::<_, RecorderError>((texture, remote))
                    })();
                    match prepared {
                        Ok((texture, remote)) if remote == slot => {
                            textures.push(texture);
                            shared_textures.push(None);
                            Reply::Registered(slot)
                        }
                        Ok(_) => Reply::Error(io_error("register", "remote slot mismatch")),
                        Err(error) => Reply::Error(error),
                    }
                }
            }
            Request::RegisterShared {
                slot,
                generation: incoming,
                adapter_luid,
                handle,
            } => {
                // Consume/close the duplicated NT handle even when request identity
                // is invalid. No user-supplied name or global object is accepted.
                let imported = import_helper_shared(&device, handle, config.width, config.height);
                if incoming != generation
                    || adapter_luid != adapter.luid
                    || slot != textures.len() as u32
                    || textures.len() >= 4
                {
                    Reply::Error(io_error(
                        "shared register",
                        "generation/adapter/slot mismatch",
                    ))
                } else {
                    let prepared = imported.and_then(|shared| {
                        // The MFT retains only helper-owned NV12, never this shared
                        // resource. Copying into a private BGRA intermediate avoids
                        // assuming the driver's shared-NV12 or mutex behavior.
                        let texture =
                            helper_input_texture(&device, &config, MfTransferFormat::Bgra8)?;
                        let remote = helper_register(&mut driver, &device, &texture, &config)?;
                        Ok((shared, texture, remote))
                    });
                    match prepared {
                        Ok((shared, texture, remote)) if remote == slot => {
                            textures.push(texture);
                            shared_textures.push(Some(shared));
                            Reply::Registered(slot)
                        }
                        Ok(_) => Reply::Error(io_error("shared register", "remote slot mismatch")),
                        Err(error) => Reply::SharedUnavailable(error),
                    }
                }
            }
            Request::Unregister { slot } => match driver.unregister_input(slot) {
                Ok(()) => {
                    if let Some(shared) = shared_textures.get_mut(slot as usize) {
                        shared.take();
                    }
                    Reply::Done
                }
                Err(error) => Reply::Error(error),
            },
            Request::Submit {
                slot,
                pts,
                duration,
                idr,
                bytes,
            } => {
                if layout.bytes != bytes as usize
                    || bytes as usize > MAX_FRAME_BYTES
                    || pixels.len() != bytes as usize
                    || shared_textures
                        .get(slot as usize)
                        .is_none_or(Option::is_some)
                {
                    return 2;
                }
                let started = transfer.as_ref().map(|_| Instant::now());
                if input.read_exact(&mut pixels).is_err() {
                    return 2;
                }
                if let Some(transfer) = transfer.as_mut() {
                    transfer.received_frames = transfer.received_frames.saturating_add(1);
                    transfer.received_bytes =
                        transfer.received_bytes.saturating_add(u64::from(bytes));
                    transfer
                        .receive
                        .record(started.expect("enabled timing").elapsed());
                }
                if let Some(texture) = textures.get(slot as usize) {
                    let started = transfer.as_ref().map(|_| Instant::now());
                    // UpdateSubresource snapshots the CPU source before returning;
                    // the next request can overwrite this helper-owned buffer.
                    unsafe {
                        context.UpdateSubresource(
                            texture,
                            0,
                            None,
                            pixels.as_ptr().cast(),
                            layout.row_bytes as u32,
                            0,
                        );
                        context.Flush();
                    }
                    if let Some(transfer) = transfer.as_mut() {
                        transfer
                            .upload
                            .record(started.expect("enabled timing").elapsed());
                    }
                    submit_reply(driver.submit(slot, pts, duration, idr))
                } else {
                    Reply::Error(io_error("submit", "unknown slot"))
                }
            }
            Request::SubmitShared {
                slot,
                generation: incoming,
                sequence,
                pts,
                duration,
                idr,
            } => {
                let result = (|| {
                    let shared = shared_textures
                        .get_mut(slot as usize)
                        .and_then(Option::as_mut)
                        .ok_or_else(|| io_error("shared submit", "unknown GPU registration"))?;
                    validate_shared_sequence(incoming, generation, sequence, shared.sequence)?;
                    let local = textures
                        .get(slot as usize)
                        .ok_or_else(|| io_error("shared submit", "unknown local texture"))?;
                    let started = transfer.as_ref().map(|_| Instant::now());
                    acquire_shared(&shared.mutex, 1)?;
                    unsafe {
                        context.CopyResource(local, &shared.texture);
                        context.Flush();
                    }
                    // Release after enqueuing the GPU read. Keyed synchronization
                    // orders GPU access; parent may publish its next frame after ACK.
                    unsafe { shared.mutex.ReleaseSync(0) }
                        .map_err(|e| io_error("shared consumer release", e))?;
                    shared.sequence = sequence;
                    if let Some(stats) = &mut transfer {
                        stats.received_frames += 1;
                        stats
                            .shared_gpu_copy
                            .record(started.expect("enabled timing").elapsed());
                    }
                    driver.submit(slot, pts, duration, idr)
                })();
                submit_reply(result)
            }
            Request::Collect {
                slot,
                wait_ms,
                capacity,
            } => match driver.collect(
                slot,
                wait_ms.min(5),
                capacity.min(super::MAX_ACCESS_UNIT_BYTES as usize),
            ) {
                Ok(NativeCollect::Pending) => Reply::Pending,
                Ok(NativeCollect::Ready(packet)) => Reply::Packet(packet),
                Err(error) => Reply::Error(error),
            },
            Request::BeginEos => match driver.begin_eos() {
                Ok(NativeEos::Submitted) => Reply::Done,
                Ok(NativeEos::Busy) => Reply::Busy,
                Err(error) => Reply::Error(error),
            },
            Request::PollEos { wait_ms } => match driver.poll_eos(wait_ms.min(5)) {
                Ok(value) => Reply::Bool(value),
                Err(error) => Reply::Error(error),
            },
            Request::Close => {
                let result = driver.close();
                let unsafe_native = driver.cleanup_unsafe();
                let reply = Reply::Closed {
                    error: result.err(),
                    unsafe_native,
                };
                let _ = write_session_reply(&mut output, reply, Some(driver.stats()), transfer);
                if unsafe_native {
                    std::process::exit(3);
                }
                return 0;
            }
            Request::Open { .. } | Request::Qualify { .. } | Request::Prepare { .. } => {
                Reply::Error(io_error("request", "duplicate open"))
            }
        };
        if write_session_reply(&mut output, reply, Some(driver.stats()), transfer).is_err() {
            return 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_mutex_wait_codes_are_never_treated_as_acquired() {
        assert!(classify_shared_acquire(0).is_ok());
        assert!(
            classify_shared_acquire(0x102)
                .unwrap_err()
                .message
                .contains("timeout")
        );
        assert!(
            classify_shared_acquire(0x80)
                .unwrap_err()
                .message
                .contains("abandoned")
        );
        assert!(classify_shared_acquire(0x887A0005).is_err());
    }

    #[test]
    fn shared_generation_sequence_rejects_replay_stale_session_and_overflow() {
        assert!(validate_shared_sequence(7, 7, 1, 0).is_ok());
        assert!(validate_shared_sequence(7, 7, 2, 1).is_ok());
        for (generation, sequence, previous) in
            [(0, 1, 0), (8, 1, 0), (7, 1, 1), (7, 3, 1), (7, 0, u64::MAX)]
        {
            assert!(validate_shared_sequence(generation, 7, sequence, previous).is_err());
        }
    }

    #[test]
    fn packed_nv12_planes_preserve_row_and_chroma_order_without_padding() {
        let layout = FrameLayout::new(4, 4, MfTransferFormat::Nv12).unwrap();
        let expected = [
            16, 16, 16, 16, 64, 64, 64, 64, 128, 128, 128, 128, 235, 235, 235, 235, 90, 240, 90,
            240, 240, 110, 240, 110,
        ];
        let mut source = [0xEE; 36];
        for (row, pixels) in expected.chunks_exact(4).enumerate() {
            source[row * 6..row * 6 + 4].copy_from_slice(pixels);
        }
        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: source.as_mut_ptr().cast(),
            RowPitch: 6,
            DepthPitch: 36,
        };
        let mut destination = allocate_frame_buffer(layout.bytes).unwrap();
        unsafe { copy_frame_rows(&mapped, layout.row_bytes, layout.rows, &mut destination) }
            .unwrap();
        assert_eq!(destination, expected);
        // A truncated plane must be rejected before any destination write.
        destination.fill(9);
        assert!(
            unsafe {
                copy_frame_rows(
                    &mapped,
                    layout.row_bytes,
                    layout.rows,
                    &mut destination[..20],
                )
            }
            .is_err()
        );
        assert_eq!(destination, [9; 24]);
    }

    #[test]
    fn format_geometry_is_bounded_and_nv12_rejects_unpaired_chroma_pixels() {
        let bgra = FrameLayout::new(1920, 1080, MfTransferFormat::Bgra8).unwrap();
        let nv12 = FrameLayout::new(1920, 1080, MfTransferFormat::Nv12).unwrap();
        assert_eq!(bgra.bytes, 8_294_400);
        assert_eq!(nv12.bytes, 3_110_400);
        assert!(FrameLayout::new(3, 4, MfTransferFormat::Nv12).is_err());
        assert!(FrameLayout::new(4, 3, MfTransferFormat::Nv12).is_err());
        assert!(FrameLayout::new(0, 4, MfTransferFormat::Nv12).is_err());
        assert!(FrameLayout::new(u32::MAX, u32::MAX, MfTransferFormat::Bgra8).is_err());
        assert!(FrameLayout::new(u32::MAX - 1, u32::MAX - 1, MfTransferFormat::Nv12).is_err());
    }

    #[test]
    fn reused_bgra_rows_overwrite_pixels_without_copying_padding() {
        let mut destination = allocate_frame_buffer(16).unwrap();
        let address = destination.as_ptr();
        for value in [7u8, 19, 0] {
            let mut source = [0xEEu8; 24];
            source[..8].fill(value);
            source[12..20].fill(value);
            let mapped = D3D11_MAPPED_SUBRESOURCE {
                pData: source.as_mut_ptr().cast(),
                RowPitch: 12,
                DepthPitch: 24,
            };
            unsafe { copy_frame_rows(&mapped, 8, 2, &mut destination) }.unwrap();
            assert_eq!(destination, [value; 16]);
            assert_eq!(destination.as_ptr(), address);
        }
    }

    #[test]
    fn invalid_mapping_and_frame_bounds_preserve_the_buffer() {
        assert!(allocate_frame_buffer(0).is_err());
        assert!(allocate_frame_buffer(MAX_FRAME_BYTES + 1).is_err());
        let mut destination = vec![9u8; 16];
        let mut source = [1u8; 24];
        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: source.as_mut_ptr().cast(),
            RowPitch: 7,
            DepthPitch: 24,
        };
        assert!(unsafe { copy_frame_rows(&mapped, 8, 2, &mut destination) }.is_err());
        assert!(
            unsafe {
                copy_frame_rows(&D3D11_MAPPED_SUBRESOURCE::default(), 8, 2, &mut destination)
            }
            .is_err()
        );
        assert_eq!(destination, [9; 16]);
    }

    // This libtest case is also a tiny child process. The parent waits for READY
    // because libtest writes its own banner before the binary protocol begins.
    #[test]
    fn fixture_process() {
        let Ok(mode) = std::env::var("FRAME_MF_IPC_FIXTURE") else {
            return;
        };
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        output.write_all(b"FRAME_MF_READY\n").unwrap();
        output.flush().unwrap();
        let request: Request = read_message(&mut input, MAX_HEADER).unwrap();
        if mode == "shared_handle_cleanup" {
            use windows::Win32::Foundation::GetHandleInformation;
            let Request::RegisterShared { handle, .. } = request else {
                panic!("expected shared handle")
            };
            let handle = OwnedNtHandle(HANDLE(handle as usize as *mut c_void));
            let mut flags = 0u32;
            unsafe { GetHandleInformation(handle.0, &mut flags) }.unwrap();
            let raw = handle.0;
            drop(handle);
            assert!(unsafe { GetHandleInformation(raw, &mut flags) }.is_err());
            write_reply(
                &mut output,
                Reply::SharedUnavailable(io_error("fixture import", "unsupported")),
                None,
            )
            .unwrap();
            let request: Request = read_message(&mut input, MAX_HEADER).unwrap();
            assert!(matches!(request, Request::Close));
            write_reply(
                &mut output,
                Reply::Closed {
                    error: None,
                    unsafe_native: false,
                },
                None,
            )
            .unwrap();
            return;
        }
        assert!(matches!(request, Request::BeginEos));
        if mode == "stall" {
            // The watchdog must interrupt this pipe read by terminating the child.
            std::thread::sleep(Duration::from_secs(30));
            std::process::exit(9);
        }
        write_reply(
            &mut output,
            Reply::Done,
            Some(NativeStats {
                native_calls: 7,
                busy_results: 1,
                pending_inputs: 2,
                ..Default::default()
            }),
        )
        .unwrap();
        let request: Request = read_message(&mut input, MAX_HEADER).unwrap();
        assert!(matches!(request, Request::PollEos { .. }));
        write_reply(
            &mut output,
            Reply::Bool(true),
            Some(NativeStats {
                native_calls: 9,
                busy_results: 1,
                pending_inputs: 0,
                ..Default::default()
            }),
        )
        .unwrap();
        let request: Request = read_message(&mut input, MAX_HEADER).unwrap();
        assert!(matches!(request, Request::Close));
        if mode == "dual_failure" {
            write_reply(
                &mut output,
                Reply::Closed {
                    error: Some(io_error("shutdown", "both native routes failed")),
                    unsafe_native: true,
                },
                Some(NativeStats {
                    native_calls: 11,
                    busy_results: 1,
                    pending_inputs: 0,
                    ..Default::default()
                }),
            )
            .unwrap();
            // Model the production child process boundary: no native owner is
            // dropped after both shutdown routes fail.
            std::process::exit(3);
        }
        write_reply(
            &mut output,
            Reply::Closed {
                error: None,
                unsafe_native: false,
            },
            Some(NativeStats {
                native_calls: 10,
                busy_results: 1,
                pending_inputs: 0,
                ..Default::default()
            }),
        )
        .unwrap();
    }

    fn fixture_channel(mode: &str) -> Channel {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(exe)
            .creation_flags(CREATE_NO_WINDOW.0)
            .arg("--exact")
            .arg("encode::mf_process::tests::fixture_process")
            .arg("--nocapture")
            .env("FRAME_MF_IPC_FIXTURE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8];
            stdout.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
            if byte[0] == b'\n' {
                if line == b"FRAME_MF_READY\n" {
                    break;
                }
                line.clear();
            }
            assert!(line.len() < 1024);
        }
        Channel::from_child(child).unwrap()
    }

    #[test]
    fn duplicate_nt_handle_is_owned_by_child_and_closed_on_import_failure() {
        use windows::Win32::System::Threading::CreateEventW;
        let mut channel = fixture_channel("shared_handle_cleanup");
        let source =
            OwnedNtHandle(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.unwrap());
        let remote = channel.duplicate_shared_handle(source.0).unwrap();
        // The helper duplicate stays valid after the parent's handle closes.
        drop(source);
        assert!(matches!(
            channel
                .exchange(
                    Request::RegisterShared {
                        slot: 0,
                        generation: 1,
                        adapter_luid: 0,
                        handle: remote,
                    },
                    &[],
                    CALL_LIMIT
                )
                .unwrap(),
            Reply::SharedUnavailable(_)
        ));
        let reply = channel.exchange(Request::Close, &[], CLOSE_LIMIT).unwrap();
        let status = channel.reap_after_close().unwrap();
        close_result(reply, status).unwrap();
    }

    #[test]
    fn dual_shutdown_failure_reaps_isolated_owner() {
        let mut channel = fixture_channel("dual_failure");
        assert!(matches!(
            channel
                .exchange(Request::BeginEos, &[], CALL_LIMIT)
                .unwrap(),
            Reply::Done
        ));
        assert!(matches!(
            channel
                .exchange(Request::PollEos { wait_ms: 5 }, &[], CALL_LIMIT)
                .unwrap(),
            Reply::Bool(true)
        ));
        let reply = channel.exchange(Request::Close, &[], CLOSE_LIMIT).unwrap();
        let status = channel.reap_after_close().unwrap();
        assert_eq!(status.code(), Some(3));
        assert!(
            close_result(reply, status)
                .unwrap_err()
                .message
                .contains("both native routes failed")
        );
    }

    #[test]
    fn successful_drain_and_close_reaps_helper() {
        let mut channel = fixture_channel("success");
        assert!(matches!(
            channel
                .exchange(Request::BeginEos, &[], CALL_LIMIT)
                .unwrap(),
            Reply::Done
        ));
        assert!(matches!(
            channel
                .exchange(Request::PollEos { wait_ms: 5 }, &[], CALL_LIMIT)
                .unwrap(),
            Reply::Bool(true)
        ));
        let reply = channel.exchange(Request::Close, &[], CLOSE_LIMIT).unwrap();
        let status = channel.reap_after_close().unwrap();
        close_result(reply, status).unwrap();
        assert_eq!(channel.last_stats.unwrap().native_calls, 10);
        assert_eq!(channel.last_stats.unwrap().pending_inputs, 0);
    }
    #[test]
    fn a_stalled_reply_is_interrupted_and_reaped() {
        let mut channel = fixture_channel("stall");
        let started = Instant::now();
        assert!(
            channel
                .exchange(Request::BeginEos, &[], Duration::from_millis(100))
                .is_err()
        );
        channel.terminate_and_reap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(channel.child.lock().unwrap().try_wait().unwrap().is_some());
    }
}
