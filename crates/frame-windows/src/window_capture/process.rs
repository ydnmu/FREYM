//! Parent GPU leases and an isolated WGC owner. The child never sends pixel bytes.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use frame_capture::CaptureCancellation;
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, GetExitCodeProcess, INFINITE, WaitForSingleObject,
};
use windows::core::{Interface, PCWSTR};

use super::native;
use super::{CapturePolicyEvidence, TimestampBaseEvidence};
use crate::capture::CapturePreparation;
use crate::capture::diagnostics::{Counters, NativeDescription};
use crate::capture::worker::{
    Acquired, CaptureSource, FrameObservation, ReadbackOutcome, internal_error,
};
use crate::surface::{
    GpuSurfaceSet, PayloadLifetime, SLOT_COUNT, SharedPayload, SlotState, SurfacePool,
};
use crate::window_discovery::WindowIdentity;

const IPC_LIMIT: Duration = Duration::from_secs(5);
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
enum Request {
    Init {
        window_id: String,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
        luid: u64,
        pool_id: u64,
        handles: [usize; SLOT_COUNT],
        show_cursor: bool,
    },
    Acquire {
        timeout_ms: u32,
    },
    Begin {
        slot: usize,
        generation: u64,
    },
    Poll {
        slot: usize,
        generation: u64,
    },
    Release,
    Stop,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Started {
        ticks_per_second: u64,
    },
    Frame {
        observation: FrameObservation,
        timestamp_base: Option<TimestampBaseEvidence>,
        policy: Option<CapturePolicyEvidence>,
        unpublishable: u64,
    },
    Empty,
    Accepted,
    Pending,
    Ready {
        slot: usize,
        generation: u64,
    },
    Released,
    Stopped,
    Error(RecorderError),
}

fn lost(message: impl Into<String>) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Stop this WGC session and start a new one.".into(),
    }
}

fn write_message<T: Serialize>(output: &mut impl Write, value: &T) -> Result<(), RecorderError> {
    let bytes = serde_json::to_vec(value).map_err(|error| internal_error(error.to_string()))?;
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE {
        return Err(lost("WGC helper IPC message exceeded its bound."));
    }
    output
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| output.write_all(&bytes))
        .and_then(|_| output.flush())
        .map_err(|error| lost(format!("WGC helper IPC write failed: {error}")))
}

fn read_message<T: for<'de> Deserialize<'de>>(input: &mut impl Read) -> Result<T, RecorderError> {
    let mut length = [0u8; 4];
    input
        .read_exact(&mut length)
        .map_err(|error| lost(format!("WGC helper IPC ended: {error}")))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE {
        return Err(lost("WGC helper IPC header was invalid."));
    }
    let mut bytes = vec![0u8; length];
    input
        .read_exact(&mut bytes)
        .map_err(|error| lost(format!("WGC helper IPC body ended: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| lost(format!("WGC helper IPC reply was invalid: {error}")))
}

fn await_reply(
    receiver: &Receiver<Result<Reply, RecorderError>>,
    deadline: Duration,
) -> Result<Reply, RecorderError> {
    match receiver.recv_timeout(deadline) {
        Ok(Ok(Reply::Error(error))) => Err(error),
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => Err(error),
        Err(RecvTimeoutError::Timeout) => Err(lost("WGC child IPC timed out.")),
        Err(RecvTimeoutError::Disconnected) => Err(lost("WGC child IPC disconnected.")),
    }
}

fn valid_grant(phase: SlotPhase, previous: u64, proposed: u64) -> bool {
    (phase == SlotPhase::Available && previous == proposed)
        || (phase == SlotPhase::Held && previous.checked_add(1) == Some(proposed))
}

struct OwnedHandle(HANDLE);
// Win32 kernel handles are process-wide references and may be closed or waited on
// from any thread. Ownership remains unique in this wrapper.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn acquire_mutex(mutex: &IDXGIKeyedMutex, key: u64) -> Result<(), RecorderError> {
    // The projection discards positive WAIT_ABANDONED/WAIT_TIMEOUT HRESULTs.
    // This calls its generated vtable, without defining an ABI or vtable by hand.
    let result = unsafe { (Interface::vtable(mutex).AcquireSync)(mutex.as_raw(), key, 100) };
    match result.0 as u32 {
        0 => Ok(()),
        0x80 => Err(lost(
            "WGC shared texture keyed mutex was abandoned; discard the session.",
        )),
        0x102 => Err(lost(
            "WGC shared texture keyed mutex timed out; discard the session.",
        )),
        other => Err(lost(format!(
            "WGC keyed mutex AcquireSync failed: 0x{other:08X}."
        ))),
    }
}

fn release_mutex(mutex: &IDXGIKeyedMutex, key: u64) -> Result<(), RecorderError> {
    unsafe { mutex.ReleaseSync(key) }
        .map_err(|error| lost(format!("WGC keyed mutex ReleaseSync failed: {error}")))
}

fn descriptor(width: u32, height: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
            as u32,
    }
}

type SharedSlots = (Vec<ID3D11Texture2D>, Vec<IDXGIKeyedMutex>, Vec<OwnedHandle>);

fn shared_slots(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<SharedSlots, RecorderError> {
    let mut textures = Vec::with_capacity(SLOT_COUNT);
    let mut mutexes = Vec::with_capacity(SLOT_COUNT);
    let mut handles = Vec::with_capacity(SLOT_COUNT);
    for _ in 0..SLOT_COUNT {
        let mut texture = None;
        unsafe { device.CreateTexture2D(&descriptor(width, height), None, Some(&mut texture)) }
            .map_err(|error| lost(format!("WGC shared texture allocation failed: {error}")))?;
        let texture =
            texture.ok_or_else(|| lost("WGC shared texture allocation returned none."))?;
        let mutex = texture
            .cast()
            .map_err(|error| lost(format!("WGC keyed mutex missing: {error}")))?;
        let resource: IDXGIResource1 = texture
            .cast()
            .map_err(|error| lost(format!("WGC shared resource missing: {error}")))?;
        let handle = unsafe {
            resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )
        }
        .map_err(|error| lost(format!("WGC shared handle creation failed: {error}")))?;
        textures.push(texture);
        mutexes.push(mutex);
        handles.push(OwnedHandle(handle));
    }
    Ok((textures, mutexes, handles))
}

fn duplicate(child: &Child, source: HANDLE) -> Result<HANDLE, RecorderError> {
    let mut target = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            HANDLE(child.as_raw_handle()),
            &mut target,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .map_err(|error| lost(format!("WGC handle duplication failed: {error}")))?;
    Ok(target)
}

fn duplicate_local(source: HANDLE) -> Result<HANDLE, RecorderError> {
    let mut target = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &mut target,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .map_err(|error| {
        lost(format!(
            "WGC parent process handle duplication failed: {error}"
        ))
    })?;
    Ok(target)
}

fn stop_outcome(
    reply: Result<Reply, RecorderError>,
    exit: Result<u32, RecorderError>,
) -> Result<(), RecorderError> {
    match reply {
        Err(error) => Err(error),
        Ok(Reply::Stopped) => match exit {
            Ok(0) => Ok(()),
            Ok(code) => Err(lost(format!(
                "WGC child exited abnormally after Stop: 0x{code:08X}."
            ))),
            Err(error) => Err(error),
        },
        Ok(_) => Err(lost("WGC child did not acknowledge Stop.")),
    }
}

struct Supervisor {
    process: OwnedHandle,
    job: OwnedHandle,
    backing: Mutex<Option<SharedPayload>>,
    failed: AtomicBool,
}

impl Supervisor {
    fn terminate(&self) {
        self.failed.store(true, Ordering::Release);
        let _ = unsafe { TerminateJobObject(self.job.0, 1) };
    }
    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 }
    }
}

fn clear_exited(supervisor: &Arc<Supervisor>) {
    if !release_backing_after_exit(&supervisor.backing, supervisor.exited()) {
        return;
    }
    if let Ok(mut guard) = active().lock()
        && guard
            .as_ref()
            .is_some_and(|value| Arc::ptr_eq(value, supervisor))
    {
        *guard = None;
    }
}

fn release_backing_after_exit(backing: &Mutex<Option<SharedPayload>>, exited: bool) -> bool {
    if !exited {
        return false;
    }
    if let Ok(mut backing) = backing.lock() {
        *backing = None;
    }
    true
}

static ACTIVE: OnceLock<Mutex<Option<Arc<Supervisor>>>> = OnceLock::new();

fn active() -> &'static Mutex<Option<Arc<Supervisor>>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

struct CommandMessage {
    request: Request,
    answer: Sender<Result<Reply, RecorderError>>,
}

fn actor(mut child: Child, input: Receiver<CommandMessage>) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    let Some(mut stdout) = child.stdout.take() else {
        return;
    };
    for message in input {
        let reply =
            write_message(&mut stdin, &message.request).and_then(|()| read_message(&mut stdout));
        let stop = matches!(message.request, Request::Stop) || reply.is_err();
        let _ = message.answer.send(reply);
        if stop {
            break;
        }
    }
    // The process/job handles remain in ACTIVE until process exit is confirmed.
    let _ = child.try_wait();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotPhase {
    Available,
    Pending,
    Held,
}

struct SlotProtocol {
    generation: u64,
    phase: SlotPhase,
    mutex: IDXGIKeyedMutex,
}

pub(super) struct ProxySource {
    commands: Sender<CommandMessage>,
    supervisor: Arc<Supervisor>,
    backing: Arc<GpuSurfaceSet>,
    pool: Arc<SurfacePool>,
    slots: Vec<SlotProtocol>,
    description: NativeDescription,
    acquired: bool,
    copied: bool,
    finish_result: Option<Result<(), RecorderError>>,
    ticks_per_second: u64,
    timestamp_base: Arc<OnceLock<TimestampBaseEvidence>>,
    policy: Arc<OnceLock<CapturePolicyEvidence>>,
    unpublishable_updates: Arc<AtomicU64>,
}

impl ProxySource {
    fn request(&self, request: Request) -> Result<Reply, RecorderError> {
        if self.supervisor.failed.load(Ordering::Acquire) {
            return Err(lost("WGC child exited or failed."));
        }
        let (answer, receiver) = mpsc::channel();
        self.commands
            .send(CommandMessage { request, answer })
            .map_err(|_| lost("WGC child command channel closed."))?;
        match await_reply(&receiver, IPC_LIMIT) {
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.supervisor.terminate();
                Err(error)
            }
        }
    }

    fn recycle(&mut self) -> Result<(), RecorderError> {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.phase == SlotPhase::Held && self.pool.slot_state(index) == Some(SlotState::Free)
            {
                let next = slot
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| lost("WGC slot generation overflowed."))?;
                release_mutex(
                    &slot.mutex,
                    next.checked_mul(2)
                        .ok_or_else(|| lost("WGC mutex generation overflowed."))?,
                )?;
                slot.generation = next;
                slot.phase = SlotPhase::Available;
            }
        }
        Ok(())
    }
}

impl CaptureSource for ProxySource {
    type Image = ();

    fn acquire(&mut self, timeout_ms: u32) -> Result<Option<Acquired<Self::Image>>, RecorderError> {
        if self.acquired {
            return Err(lost("WGC proxy acquired a second frame before release."));
        }
        self.recycle()?;
        match self.request(Request::Acquire { timeout_ms })? {
            Reply::Frame {
                observation,
                timestamp_base,
                policy,
                unpublishable,
            } => {
                if let Some(base) = timestamp_base {
                    let _ = self.timestamp_base.set(base);
                }
                if let Some(value) = policy {
                    let _ = self.policy.set(value);
                }
                self.unpublishable_updates
                    .store(unpublishable, Ordering::Relaxed);
                self.acquired = true;
                self.copied = false;
                Ok(Some(Acquired {
                    image: (),
                    observation,
                }))
            }
            Reply::Empty => Ok(None),
            _ => Err(lost("WGC child sent an unexpected acquire reply.")),
        }
    }

    fn begin_copy(&mut self, _: &Self::Image, slot_index: usize) -> Result<(), RecorderError> {
        // The worker claims a Free slot after acquire(). A queue eviction may
        // make it Free during process(), so perform hand-back at the grant too.
        if self
            .slots
            .get(slot_index)
            .is_some_and(|slot| slot.phase == SlotPhase::Held)
            && self.pool.slot_state(slot_index) == Some(SlotState::CopyPending)
        {
            let slot = &mut self.slots[slot_index];
            let next = slot
                .generation
                .checked_add(1)
                .and_then(|value| value.checked_mul(2).map(|key| (value, key)))
                .ok_or_else(|| lost("WGC mutex generation overflowed."))?;
            release_mutex(&slot.mutex, next.1)?;
            slot.generation = next.0;
            slot.phase = SlotPhase::Available;
        }
        let slot = self
            .slots
            .get(slot_index)
            .ok_or_else(|| lost("WGC slot index outside the four-slot pool."))?;
        if !self.acquired || slot.phase != SlotPhase::Available {
            return Err(lost(format!(
                "WGC slot {slot_index} grant is stale: acquired={} phase={} generation={} pool={:?}.",
                self.acquired,
                match slot.phase {
                    SlotPhase::Available => "available",
                    SlotPhase::Pending => "pending",
                    SlotPhase::Held => "held",
                },
                slot.generation,
                self.pool.slot_state(slot_index)
            )));
        }
        let generation = slot.generation;
        match self.request(Request::Begin {
            slot: slot_index,
            generation,
        })? {
            Reply::Accepted => {
                self.slots[slot_index].phase = SlotPhase::Pending;
                self.copied = true;
                Ok(())
            }
            _ => Err(lost("WGC child rejected the slot grant.")),
        }
    }

    fn query_complete(&mut self, slot_index: usize) -> Result<bool, RecorderError> {
        let slot = self
            .slots
            .get(slot_index)
            .ok_or_else(|| lost("WGC completion index outside the pool."))?;
        if slot.phase != SlotPhase::Pending {
            return Err(lost("WGC completion is stale or duplicated."));
        }
        let generation = slot.generation;
        match self.request(Request::Poll {
            slot: slot_index,
            generation,
        })? {
            Reply::Pending => Ok(false),
            Reply::Ready {
                slot,
                generation: received,
            } if slot == slot_index && received == generation => {
                acquire_mutex(
                    &self.slots[slot_index].mutex,
                    generation
                        .checked_mul(2)
                        .and_then(|key| key.checked_add(1))
                        .ok_or_else(|| lost("WGC mutex generation overflowed."))?,
                )?;
                self.slots[slot_index].phase = SlotPhase::Held;
                Ok(true)
            }
            _ => Err(lost(
                "WGC child Ready generation or slot did not match the grant.",
            )),
        }
    }

    fn release_frame(&mut self) -> Result<(), RecorderError> {
        if !self.acquired {
            return Err(lost("WGC proxy frame release had no acquisition."));
        }
        self.acquired = false;
        if !self.copied {
            match self.request(Request::Release)? {
                Reply::Released => Ok(()),
                _ => Err(lost("WGC child did not release the unused frame.")),
            }
        } else {
            Ok(())
        }
    }

    fn readback(&mut self, _: usize) -> ReadbackOutcome {
        ReadbackOutcome::Failed(internal_error(
            "WGC pixel readback is not implemented.".into(),
        ))
    }
    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }
    fn sample_counter(&mut self) -> Result<i64, RecorderError> {
        let mut value = 0;
        unsafe { QueryPerformanceCounter(&mut value) }
            .map_err(|error| lost(format!("WGC parent QPC failed: {error}")))?;
        Ok(value)
    }
    fn description(&self) -> &NativeDescription {
        &self.description
    }
    fn payload_owner(&self) -> SharedPayload {
        Arc::clone(&self.backing) as SharedPayload
    }
    fn preview_device(&self) -> Option<ID3D11Device> {
        Some(self.backing.device())
    }
    fn preview_context(&self) -> Option<ID3D11DeviceContext> {
        unsafe { self.backing.device().GetImmediateContext() }.ok()
    }
    fn preview_source(&self, slot: usize) -> Option<crate::preview::PreviewSource> {
        // The worker owns the completed parent slot's keyed mutex until publication.
        Some(crate::preview::PreviewSource::new(
            self.backing.texture(slot)?,
            self.description.width,
            self.description.height,
        ))
    }
    fn finish(&mut self) -> Result<(), RecorderError> {
        if let Some(result) = &self.finish_result {
            return result.clone();
        }
        let deadline = Instant::now() + Duration::from_secs(7);
        let (answer, receiver) = mpsc::channel();
        let reply = self
            .commands
            .send(CommandMessage {
                request: Request::Stop,
                answer,
            })
            .map_err(|_| lost("WGC child command channel closed before Stop."))
            .and_then(|()| {
                await_reply(
                    &receiver,
                    deadline.saturating_duration_since(Instant::now()),
                )
            });
        if reply
            .as_ref()
            .is_err_and(|error| error.message.contains("timed out"))
        {
            self.supervisor.terminate();
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_ms = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        let exit = if unsafe { WaitForSingleObject(self.supervisor.process.0, wait_ms) }
            == WAIT_OBJECT_0
        {
            let mut code = 0u32;
            unsafe { GetExitCodeProcess(self.supervisor.process.0, &mut code) }
                .map(|()| code)
                .map_err(|error| lost(format!("WGC child exit code unavailable: {error}")))
        } else {
            self.supervisor.terminate();
            Err(lost("WGC child did not exit within the Stop deadline."))
        };
        clear_exited(&self.supervisor);
        let result = stop_outcome(reply, exit);
        self.finish_result = Some(result.clone());
        result
    }
}

impl Drop for ProxySource {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn assign_job(child: &Child) -> Result<OwnedHandle, RecorderError> {
    let raw = unsafe { CreateJobObjectW(None, None) }
        .map_err(|error| lost(format!("WGC job creation failed: {error}")))?;
    let job = OwnedHandle(raw);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of_val(&limits) as u32,
        )
    }
    .map_err(|error| lost(format!("WGC job configuration failed: {error}")))?;
    unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) }
        .map_err(|error| lost(format!("WGC job assignment failed: {error}")))?;
    Ok(job)
}

pub(super) fn open(
    identity: &WindowIdentity,
    source_geometry: (u32, u32),
    output_geometry: (u32, u32),
    timestamp_base: &Arc<OnceLock<TimestampBaseEvidence>>,
    policy: &Arc<OnceLock<CapturePolicyEvidence>>,
    unpublishable_updates: &Arc<AtomicU64>,
    preparation: &CapturePreparation<'_>,
) -> Result<ProxySource, RecorderError> {
    if preparation.verify_pixels {
        return Err(internal_error(
            "WGC cross-process readback is unavailable.".into(),
        ));
    }
    let mut active = active()
        .lock()
        .map_err(|_| lost("WGC supervisor lock poisoned."))?;
    if let Some(previous) = active.as_ref()
        && !previous.exited()
    {
        return Err(lost(
            "The previous WGC helper has not exited; a new session is refused.",
        ));
    }
    *active = None;
    let (device, luid, adapter_description) = native::parent_device()?;
    let (textures, mutexes, handles) = shared_slots(&device, output_geometry.0, output_geometry.1)?;
    let backing = GpuSurfaceSet::new(
        device,
        textures,
        output_geometry.0,
        output_geometry.1,
        preparation.pool_id,
        Arc::clone(preparation.lifetime),
    );
    let exe = std::env::current_exe()
        .map_err(|error| lost(format!("WGC executable lookup failed: {error}")))?;
    let mut child = Command::new(exe)
        .arg("--frame-wgc-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|error| lost(format!("WGC helper spawn failed: {error}")))?;
    let job = match assign_job(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let process = duplicate_local(HANDLE(child.as_raw_handle()))?;
    let supervisor = Arc::new(Supervisor {
        process: OwnedHandle(process),
        job,
        backing: Mutex::new(Some(Arc::clone(&backing) as SharedPayload)),
        failed: AtomicBool::new(false),
    });
    *active = Some(Arc::clone(&supervisor));
    // Initialization may fail and drop ProxySource, whose finish path takes
    // ACTIVE again. Keep the reservation but release the lock before IPC.
    drop(active);
    let watcher = Arc::clone(&supervisor);
    if let Err(error) = std::thread::Builder::new()
        .name("frame-wgc-supervisor".into())
        .spawn(move || {
            let _ = unsafe { WaitForSingleObject(watcher.process.0, INFINITE) };
            clear_exited(&watcher);
        })
    {
        supervisor.terminate();
        let _ = unsafe { WaitForSingleObject(supervisor.process.0, 500) };
        clear_exited(&supervisor);
        return Err(lost(format!(
            "WGC supervisor watcher spawn failed: {error}"
        )));
    }
    let mut child_handles = [0usize; SLOT_COUNT];
    for (index, handle) in handles.iter().enumerate() {
        match duplicate(&child, handle.0) {
            Ok(value) => child_handles[index] = value.0 as usize,
            Err(error) => {
                supervisor.terminate();
                return Err(error);
            }
        }
    }
    let (commands, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("frame-wgc-ipc".into())
        .spawn(move || actor(child, receiver))
        .map_err(|error| {
            supervisor.terminate();
            lost(format!("WGC IPC actor spawn failed: {error}"))
        })?;
    let slots = mutexes
        .into_iter()
        .map(|mutex| SlotProtocol {
            generation: 0,
            phase: SlotPhase::Available,
            mutex,
        })
        .collect();
    let mut source = ProxySource {
        commands,
        supervisor,
        backing,
        pool: Arc::clone(preparation.pool),
        slots,
        description: NativeDescription {
            api: "Windows.Graphics.Capture isolated child + parent shared D3D11 slots".into(),
            adapter_description,
            output_gdi_name: identity.id().0,
            width: output_geometry.0,
            height: output_geometry.1,
            pixel_format: "DXGI_FORMAT_B8G8R8A8_UNORM".into(),
            rotation_identity: true,
            desktop_image_in_system_memory: false,
            context_multithread_protected: true,
            staging_bytes: 0,
        },
        acquired: false,
        copied: false,
        finish_result: None,
        ticks_per_second: 0,
        timestamp_base: Arc::clone(timestamp_base),
        policy: Arc::clone(policy),
        unpublishable_updates: Arc::clone(unpublishable_updates),
    };
    let reply = source.request(Request::Init {
        window_id: identity.id().0,
        source_width: source_geometry.0,
        source_height: source_geometry.1,
        output_width: output_geometry.0,
        output_height: output_geometry.1,
        luid,
        pool_id: preparation.pool_id,
        handles: child_handles,
        show_cursor: preparation.show_cursor,
    })?;
    match reply {
        Reply::Started { ticks_per_second } if ticks_per_second != 0 => {
            source.ticks_per_second = ticks_per_second;
            Ok(source)
        }
        _ => {
            source.supervisor.terminate();
            Err(lost("WGC helper did not confirm startup."))
        }
    }
}

fn child_run() -> Result<(), RecorderError> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let Request::Init {
        window_id,
        source_width,
        source_height,
        output_width,
        output_height,
        luid,
        pool_id,
        handles,
        show_cursor,
    } = read_message(&mut input)?
    else {
        return Err(lost("WGC helper expected Init."));
    };
    let owned_handles = handles.map(|raw| OwnedHandle(HANDLE(raw as *mut c_void)));
    let identity = WindowIdentity::parse(&window_id)?;
    let cancellation = CaptureCancellation::default();
    let lifetime = PayloadLifetime::new();
    let counters = Arc::new(Counters::default());
    let pool = SurfacePool::new(0);
    let preparation = CapturePreparation {
        defer_frames: false,
        cancellation: &cancellation,
        verify_pixels: false,
        show_cursor,
        lifetime: &lifetime,
        counters: &counters,
        pool_id,
        pool: &pool,
    };
    let timestamp_base = Arc::new(OnceLock::new());
    let policy = Arc::new(OnceLock::new());
    let unpublishable_updates = Arc::new(AtomicU64::new(0));
    let mut source = native::open_shared(
        &identity,
        native::CaptureGeometry {
            source: (source_width, source_height),
            output: (output_width, output_height),
        },
        &timestamp_base,
        &policy,
        &unpublishable_updates,
        &preparation,
        (owned_handles.each_ref().map(|handle| handle.0), luid),
    )?;
    let mutexes: Vec<_> = (0..SLOT_COUNT)
        .map(|index| source.slot_mutex(index))
        .collect::<Result<_, _>>()?;
    let mut slots = [SlotPhase::Available; SLOT_COUNT];
    let mut generations = [0u64; SLOT_COUNT];
    let mut acquired = false;
    write_message(
        &mut output,
        &Reply::Started {
            ticks_per_second: source.ticks_per_second(),
        },
    )?;
    while let Ok(request) = read_message::<Request>(&mut input) {
        let reply: Result<Reply, RecorderError> = match request {
            Request::Init { .. } => Err(lost("WGC helper received duplicate Init.")),
            Request::Acquire { timeout_ms } => {
                if acquired {
                    Err(lost("WGC helper received duplicate Acquire."))
                } else {
                    source.acquire(timeout_ms).map(|frame| match frame {
                        Some(Acquired { observation, .. }) => {
                            acquired = true;
                            Reply::Frame {
                                observation,
                                timestamp_base: timestamp_base.get().copied(),
                                policy: policy.get().copied(),
                                unpublishable: unpublishable_updates.load(Ordering::Relaxed),
                            }
                        }
                        None => Reply::Empty,
                    })
                }
            }
            Request::Begin { slot, generation } => {
                if !acquired
                    || slot >= SLOT_COUNT
                    || !valid_grant(slots[slot], generations[slot], generation)
                {
                    Err(lost("WGC helper received a stale slot grant."))
                } else {
                    let key = generation
                        .checked_mul(2)
                        .ok_or_else(|| lost("WGC mutex key overflowed."));
                    key.and_then(|key| acquire_mutex(&mutexes[slot], key))
                        .and_then(|()| source.begin_copy(&(), slot))
                        .and_then(|()| source.release_frame())
                        .map(|()| {
                            acquired = false;
                            generations[slot] = generation;
                            slots[slot] = SlotPhase::Pending;
                            Reply::Accepted
                        })
                }
            }
            Request::Poll { slot, generation } => {
                if slot >= SLOT_COUNT
                    || slots[slot] != SlotPhase::Pending
                    || generations[slot] != generation
                {
                    Err(lost("WGC helper received a stale completion poll."))
                } else {
                    source.query_complete(slot).and_then(|complete| {
                        if complete {
                            let key = generation
                                .checked_mul(2)
                                .and_then(|key| key.checked_add(1))
                                .ok_or_else(|| lost("WGC mutex key overflowed."))?;
                            release_mutex(&mutexes[slot], key)?;
                            slots[slot] = SlotPhase::Held;
                            Ok(Reply::Ready { slot, generation })
                        } else {
                            Ok(Reply::Pending)
                        }
                    })
                }
            }
            Request::Release => {
                if !acquired {
                    Err(lost("WGC helper received an extra frame release."))
                } else {
                    acquired = false;
                    source.release_frame().map(|()| Reply::Released)
                }
            }
            Request::Stop => source.finish().map(|()| Reply::Stopped),
        };
        let stopping = matches!(request, Request::Stop) || reply.is_err();
        match reply {
            Ok(reply) => write_message(&mut output, &reply)?,
            Err(error) => {
                let _ = write_message(&mut output, &Reply::Error(error));
                break;
            }
        }
        if stopping {
            break;
        }
    }
    let _ = source.finish();
    Ok(())
}

pub(super) fn run_helper() -> i32 {
    match child_run() {
        Ok(()) => 0,
        Err(error) => {
            let _ = write_message(&mut std::io::stdout(), &Reply::Error(error));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn helper_init_retains_the_explicit_cursor_preference() {
        for show_cursor in [false, true] {
            let init = Request::Init {
                window_id: "fixture".into(),
                source_width: 16,
                source_height: 16,
                output_width: 16,
                output_height: 16,
                luid: 1,
                pool_id: 1,
                handles: [0; SLOT_COUNT],
                show_cursor,
            };
            let encoded = serde_json::to_vec(&init).unwrap();
            let Request::Init {
                show_cursor: decoded,
                ..
            } = serde_json::from_slice(&encoded).unwrap()
            else {
                panic!("expected helper Init");
            };
            assert_eq!(decoded, show_cursor);
        }
    }
    use super::*;
    use crate::surface::{EncodeTexture, PayloadOwner};
    use std::sync::atomic::AtomicUsize;

    struct TestPayload(Arc<AtomicUsize>);

    impl PayloadOwner for TestPayload {
        fn encode_texture(&self, _: usize) -> Option<EncodeTexture> {
            None
        }
    }

    impl Drop for TestPayload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn grant_requires_lease_return_and_next_generation() {
        assert!(valid_grant(SlotPhase::Available, 0, 0));
        assert!(!valid_grant(SlotPhase::Pending, 0, 1));
        assert!(!valid_grant(SlotPhase::Held, 0, 0));
        assert!(valid_grant(SlotPhase::Held, 0, 1));
        assert!(!valid_grant(SlotPhase::Held, 0, 2));
        assert!(!valid_grant(SlotPhase::Held, u64::MAX, 0));
    }

    #[test]
    fn ipc_eof_and_timeout_fail_closed() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let eof: Result<Request, _> = read_message(&mut empty);
        assert_eq!(eof.err().unwrap().code, ErrorCode::SourceLost);

        let (sender, receiver) = mpsc::channel();
        let timeout = await_reply(&receiver, Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(timeout.code, ErrorCode::SourceLost);
        assert!(timeout.message.contains("timed out"));
        drop(sender);
        let disconnected = await_reply(&receiver, Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(disconnected.code, ErrorCode::SourceLost);
        assert!(disconnected.message.contains("disconnected"));
    }

    #[test]
    fn stop_needs_ack_and_zero_exit_while_preserving_child_error() {
        assert!(stop_outcome(Ok(Reply::Stopped), Ok(0)).is_ok());
        assert_eq!(
            stop_outcome(Ok(Reply::Stopped), Ok(0xC0000005))
                .err()
                .unwrap()
                .code,
            ErrorCode::SourceLost
        );
        let native = internal_error("WGC native finish failed.".into());
        let result = stop_outcome(Err(native.clone()), Ok(1)).err().unwrap();
        assert_eq!(result, native);
        assert_eq!(
            stop_outcome(Ok(Reply::Pending), Ok(0)).err().unwrap().code,
            ErrorCode::SourceLost
        );
    }

    #[test]
    fn timeout_retains_parent_payload_until_exit_is_confirmed() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let backing: Mutex<Option<SharedPayload>> =
            Mutex::new(Some(Arc::new(TestPayload(Arc::clone(&dropped)))));
        assert!(!release_backing_after_exit(&backing, false));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert!(backing.lock().unwrap().is_some());
        assert!(release_backing_after_exit(&backing, true));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
