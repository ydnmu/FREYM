//! Isolated FRAME-1303 feasibility proof; no production capture or IPC path uses it.

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::num::{NonZeroU32, NonZeroU64};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use frame_capture::CapturedFrame;
use frame_clock::MediaTimestamp;
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use frame_encode::{EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11Device1, ID3D11Query, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAS_STILL_DRAWING, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
    IDXGIKeyedMutex, IDXGIResource1,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, GetCurrentProcess, SetEvent, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows::core::{BOOL, Interface, PCWSTR};

use super::{NativeEncoderConfig, WindowsH264Encoder};
use crate::surface::{GpuSurfaceSet, PayloadLifetime, SharedPayload, SurfacePool, WindowsSurface};

type ProofResult<T> = Result<T, Box<dyn std::error::Error>>;
const WAIT_MS: u32 = 5_000;
const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const PIXEL: [u8; 4] = [37, 91, 173, 255];

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this value was returned by CreateEventW/CreateSharedHandle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn mutex_acquire(mutex: &IDXGIKeyedMutex, key: u64, timeout_ms: u32) -> ProofResult<()> {
    // SAFETY: the official windows binding's Result<()> erases the positive
    // WAIT_TIMEOUT/WAIT_ABANDONED status codes. Call its generated vtable so both
    // statuses remain distinguishable; no ABI or vtable layout is handwritten.
    let status = unsafe { (Interface::vtable(mutex).AcquireSync)(mutex.as_raw(), key, timeout_ms) };
    match status.0 as u32 {
        0 => Ok(()),
        0x80 => Err("WAIT_ABANDONED: shared texture and keyed mutex must be recreated".into()),
        0x102 => Err("WAIT_TIMEOUT: shared texture ownership did not transfer".into()),
        _ => Err(windows::core::Error::from(status).into()),
    }
}

fn mutex_release(mutex: &IDXGIKeyedMutex, key: u64) -> ProofResult<()> {
    // SAFETY: called only by the device currently holding this mutex.
    unsafe { mutex.ReleaseSync(key) }?;
    Ok(())
}

fn descriptor() -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
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

fn create_shared(device: &ID3D11Device) -> ProofResult<(ID3D11Texture2D, OwnedHandle)> {
    let mut texture = None;
    // SAFETY: initialized descriptor, no initial data, owned out-parameter.
    unsafe { device.CreateTexture2D(&descriptor(), None, Some(&mut texture)) }?;
    let texture = texture.ok_or("CreateTexture2D returned no shared texture")?;
    let resource: IDXGIResource1 = texture.cast()?;
    // SAFETY: NT shared handle from a texture with both required share flags.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )
    }?;
    Ok((texture, OwnedHandle(handle)))
}

fn duplicate_into(child: &Child, source: HANDLE) -> ProofResult<HANDLE> {
    let mut target = HANDLE::default();
    // SAFETY: source/target process handles are live and destination is a valid out-parameter.
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
    }?;
    Ok(target)
}

fn child_process(
    mode: &str,
    luid: u64,
    texture: HANDLE,
) -> ProofResult<(Child, OwnedHandle, OwnedHandle)> {
    let ready = OwnedHandle(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }?);
    let done = OwnedHandle(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }?);
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--shared-texture-child")
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let setup = (|| -> ProofResult<()> {
        let target_texture = duplicate_into(&child, texture)?;
        let target_ready = duplicate_into(&child, ready.0)?;
        let target_done = duplicate_into(&child, done.0)?;
        writeln!(
            child.stdin.as_mut().ok_or("child stdin missing")?,
            "{} {} {} {luid} {mode}",
            target_texture.0 as usize,
            target_ready.0 as usize,
            target_done.0 as usize,
        )?;
        Ok(())
    })();
    if let Err(error) = setup {
        let cleanup = stop_child(&mut child);
        return Err(format!("child setup failed: {error}; cleanup: {cleanup:?}").into());
    }
    Ok((child, ready, done))
}

fn process_handle(child: &Child) -> HANDLE {
    HANDLE(child.as_raw_handle())
}

fn wait_ready_or_exit(child: &Child, ready: HANDLE) -> ProofResult<()> {
    // SAFETY: both handles are live; the wait is bounded and checks child exit too.
    let status = unsafe { WaitForMultipleObjects(&[ready, process_handle(child)], false, WAIT_MS) };
    if status == WAIT_OBJECT_0 {
        Ok(())
    } else if status.0 == WAIT_OBJECT_0.0 + 1 {
        Err("SourceLost: child exited before signaling texture readiness".into())
    } else if status == WAIT_TIMEOUT {
        Err("SourceLost: child texture readiness timed out".into())
    } else {
        Err(format!("WaitForMultipleObjects failed: 0x{:08X}", status.0).into())
    }
}

fn remaining_ms(deadline: Instant) -> u32 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(u128::from(u32::MAX)) as u32
}

fn wait_child_exit(child: &mut Child) -> ProofResult<()> {
    // SAFETY: the child process handle remains owned by Child for this bounded wait.
    let status = unsafe { WaitForSingleObject(process_handle(child), WAIT_MS) };
    if status != WAIT_OBJECT_0 {
        return Err("child did not exit within five seconds; GPU cleanup is unverified".into());
    }
    let exit = child
        .try_wait()?
        .ok_or("signaled child still has no exit status")?;
    if !exit.success() {
        return Err(format!("SourceLost: child exited with {exit}").into());
    }
    Ok(())
}

fn stop_child(child: &mut Child) -> ProofResult<()> {
    stop_child_before(
        child,
        Instant::now() + Duration::from_millis(WAIT_MS as u64),
    )
}

fn stop_child_before(child: &mut Child, deadline: Instant) -> ProofResult<()> {
    let _ = child.kill();
    // TerminateProcess is asynchronous; a signaled process handle is the proof of exit.
    let status = unsafe { WaitForSingleObject(process_handle(child), remaining_ms(deadline)) };
    if status != WAIT_OBJECT_0 {
        return Err("child termination did not complete before the transaction deadline".into());
    }
    let _ = child.try_wait()?;
    Ok(())
}

fn child_run() -> ProofResult<()> {
    let mut command = String::new();
    BufReader::new(std::io::stdin()).read_line(&mut command)?;
    let words: Vec<_> = command.split_whitespace().collect();
    if words.len() != 5 {
        return Err("invalid shared-texture child command".into());
    }
    let texture_handle = OwnedHandle(HANDLE(words[0].parse::<usize>()? as *mut c_void));
    let ready = OwnedHandle(HANDLE(words[1].parse::<usize>()? as *mut c_void));
    let done = OwnedHandle(HANDLE(words[2].parse::<usize>()? as *mut c_void));
    let luid = words[3].parse::<u64>()?;
    let mode = words[4];
    let device = super::native::qualification_device_for_luid(luid)?;
    let device1: ID3D11Device1 = device.cast()?;
    // SAFETY: duplicated NT handle names a shared texture on this same LUID.
    let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(texture_handle.0) }?;
    let mutex: IDXGIKeyedMutex = texture.cast()?;
    mutex_acquire(&mutex, 0, WAIT_MS)?;
    if mode == "die" {
        // Signal while holding the lock, then terminate without ReleaseSync.
        unsafe { SetEvent(ready.0) }?;
        std::process::exit(42);
    }
    if mode == "stall" {
        // Deliberate user-mode stall with no Ready/ACK. Only the parent watchdog
        // can end this process; no GPU/TDR stall is induced.
        loop {
            std::thread::park();
        }
    }
    if mode != "normal" {
        return Err("unknown child mode".into());
    }
    let context = unsafe { device.GetImmediateContext() }?;
    let resource: ID3D11Resource = texture.cast()?;
    let pixels = vec![PIXEL; (WIDTH * HEIGHT) as usize];
    // SAFETY: the source covers WIDTH*HEIGHT BGRA pixels and remains alive through
    // UpdateSubresource; the shared mutex is held by this child device.
    unsafe {
        context.UpdateSubresource(&resource, 0, None, pixels.as_ptr().cast(), WIDTH * 4, 0);
    }
    let mut query: Option<ID3D11Query> = None;
    unsafe {
        device.CreateQuery(
            &D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            },
            Some(&mut query),
        )
    }?;
    let query = query.ok_or("CreateQuery returned no event query")?;
    unsafe {
        context.End(&query);
        context.Flush()
    };
    let until = Instant::now() + Duration::from_millis(WAIT_MS as u64);
    loop {
        let mut complete = BOOL(0);
        let result = unsafe {
            context.GetData(
                &query,
                Some((&mut complete as *mut BOOL).cast()),
                std::mem::size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
        };
        result?;
        if complete.as_bool() {
            break;
        }
        if Instant::now() >= until {
            return Err("child GPU update query timed out".into());
        }
        std::thread::yield_now();
    }
    mutex_release(&mutex, 1)?;
    unsafe { SetEvent(ready.0) }?;
    let status = unsafe { WaitForSingleObject(done.0, WAIT_MS) };
    if status != WAIT_OBJECT_0 {
        return Err("parent did not return texture slot".into());
    }
    mutex_acquire(&mutex, 2, WAIT_MS)?;
    mutex_release(&mutex, 0)?;
    Ok(())
}

pub(crate) fn shared_texture_child_entry() -> i32 {
    match child_run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("shared-texture child: {error}");
            2
        }
    }
}

fn verify_pixels(device: &ID3D11Device, texture: &ID3D11Texture2D) -> ProofResult<()> {
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..descriptor()
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }?;
    let staging = staging.ok_or("no diagnostic staging texture")?;
    let source: ID3D11Resource = texture.cast()?;
    let dest: ID3D11Resource = staging.cast()?;
    let context = unsafe { device.GetImmediateContext() }?;
    unsafe {
        context.CopyResource(&dest, &source);
        context.Flush()
    };
    let until = Instant::now() + Duration::from_millis(WAIT_MS as u64);
    loop {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        match unsafe {
            context.Map(
                &dest,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        } {
            Ok(()) => {
                if mapped.pData.is_null() || mapped.RowPitch < WIDTH * 4 {
                    unsafe { context.Unmap(&dest, 0) };
                    return Err("diagnostic staging map is invalid".into());
                }
                let first = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), 4) };
                let last_offset =
                    (HEIGHT as usize - 1) * mapped.RowPitch as usize + (WIDTH as usize - 1) * 4;
                let last = unsafe {
                    std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(last_offset), 4)
                };
                let valid = first == PIXEL && last == PIXEL;
                unsafe { context.Unmap(&dest, 0) };
                return if valid {
                    Ok(())
                } else {
                    Err("shared GPU pixels differed from the known BGRA pattern".into())
                };
            }
            Err(error)
                if error.code() == DXGI_ERROR_WAS_STILL_DRAWING && Instant::now() < until =>
            {
                std::thread::yield_now();
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn encode_one(
    device: ID3D11Device,
    texture: ID3D11Texture2D,
    config: NativeEncoderConfig,
) -> ProofResult<(frame_encode::EncodedStreamInfo, frame_encode::EncodedPacket)> {
    let pool = SurfacePool::new(0);
    let payload: SharedPayload = GpuSurfaceSet::new(
        device,
        vec![texture],
        WIDTH,
        HEIGHT,
        pool.id(),
        PayloadLifetime::new(),
    );
    let slot = pool
        .try_acquire_copy_slot()
        .ok_or("prototype slot unavailable")?;
    slot.mark_ready();
    slot.mark_consumer_owned();
    let surface = WindowsSurface::new(slot, payload);
    let mut encoder = WindowsH264Encoder::open(config, &surface)?;
    let info = encoder.info().clone();
    let input = EncoderInput {
        input: CapturedFrame {
            surface,
            timestamp: MediaTimestamp(1_000_000),
            width: WIDTH,
            height: HEIGHT,
        },
        duration_ns: NonZeroU64::new(16_666_667).ok_or("invalid duration")?,
    };
    match encoder.submit(input)? {
        SubmitOutcome::Accepted => {}
        SubmitOutcome::Backpressure(_) => return Err("single frame backpressured".into()),
    }
    encoder.begin_drain()?;
    let until = Instant::now() + Duration::from_secs(10);
    let mut packet = None;
    loop {
        match encoder.try_packet()? {
            EncoderPoll::Packet(value) if packet.is_none() => packet = Some(value),
            EncoderPoll::Packet(_) => return Err("single frame made multiple packets".into()),
            EncoderPoll::Drained => break,
            EncoderPoll::Pending if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(2));
            }
            EncoderPoll::Pending => return Err("encoder drain timed out".into()),
        }
    }
    encoder.stop()?;
    if pool.counts().free != crate::surface::SLOT_COUNT {
        return Err("encoder did not return the prototype lease".into());
    }
    Ok((info, packet.ok_or("encoder produced no packet")?))
}

pub(crate) fn shared_texture_parent_proof() -> ProofResult<(
    frame_encode::EncodedStreamInfo,
    frame_encode::EncodedPacket,
    String,
)> {
    let adapter = super::enumerate_native_adapters()?
        .into_iter()
        .find(|adapter| adapter.vendor_id == 0x10DE)
        .ok_or("no NVIDIA adapter for the existing NVENC consumer")?;
    let config = NativeEncoderConfig::proof(
        WIDTH,
        HEIGHT,
        NonZeroU32::new(60).ok_or("zero rate")?,
        NonZeroU32::new(1).ok_or("zero denominator")?,
    )?;
    let device = super::native::qualification_device_for_luid(adapter.luid)?;
    let (texture, shared) = create_shared(&device)?;
    let mutex: IDXGIKeyedMutex = texture.cast()?;
    let (mut child, ready, done) = child_process("normal", adapter.luid, shared.0)?;
    let started = Instant::now();
    let normal = (|| -> ProofResult<_> {
        wait_ready_or_exit(&child, ready.0)?;
        let handoff_ms = started.elapsed().as_millis();
        mutex_acquire(&mutex, 1, WAIT_MS)?;
        verify_pixels(&device, &texture)?;
        let (info, packet) = encode_one(device.clone(), texture.clone(), config)?;
        mutex_release(&mutex, 2)?;
        unsafe { SetEvent(done.0) }?;
        wait_child_exit(&mut child)?;
        Ok((info, packet, handoff_ms))
    })();
    let (info, packet, handoff_ms) = match normal {
        Ok(value) => value,
        Err(error) => {
            let stopped = stop_child(&mut child);
            return Err(format!(
                "normal shared-texture proof failed: {error}; child cleanup: {stopped:?}"
            )
            .into());
        }
    };

    let (failed_texture, failed_shared) = create_shared(&device)?;
    let failed_mutex: IDXGIKeyedMutex = failed_texture.cast()?;
    let (mut failed_child, failed_ready, _failed_done) =
        child_process("die", adapter.luid, failed_shared.0)?;
    let failure_started = Instant::now();
    if let Err(error) = wait_ready_or_exit(&failed_child, failed_ready.0) {
        let cleanup = stop_child(&mut failed_child);
        return Err(
            format!("failure-child readiness failed: {error}; cleanup: {cleanup:?}").into(),
        );
    }
    // The controlled child exits while owning key 0; confirm exit before probing
    // the abandoned mutex. Never publish or recycle this texture.
    let exit_status = unsafe { WaitForSingleObject(process_handle(&failed_child), WAIT_MS) };
    if exit_status != WAIT_OBJECT_0 {
        let cleanup = stop_child(&mut failed_child);
        return Err(format!("child failure exit unconfirmed; cleanup: {cleanup:?}").into());
    }
    let child_status = failed_child
        .try_wait()?
        .ok_or("failed child did not report exit")?;
    let abandoned = match mutex_acquire(&failed_mutex, 1, 100) {
        Ok(()) => return Err("failed child unexpectedly transferred mutex ownership".into()),
        Err(error) => error.to_string(),
    };
    if !abandoned.contains("WAIT_ABANDONED") {
        return Err(format!("unexpected failed-child mutex state: {abandoned}").into());
    }
    let source_lost = RecorderError {
        code: ErrorCode::SourceLost,
        message: "Shared-texture child exited while owning its GPU slot; discard the slot.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new capture session.".into(),
    };
    if source_lost.code != ErrorCode::SourceLost || child_status.success() {
        return Err("controlled child failure was not classified as SourceLost".into());
    }
    let evidence = format!(
        "adapter_luid={:016X} normal_handoff_ms={} child_failure_ms={} failure=SourceLost mutex=WAIT_ABANDONED slots_reused=0 child_exit={}",
        adapter.luid,
        handoff_ms,
        failure_started.elapsed().as_millis(),
        child_status,
    );
    Ok((info, packet, evidence))
}

/// One failure-only transaction. No frame is published or sent to an encoder.
pub(crate) fn shared_texture_watchdog_proof() -> ProofResult<String> {
    let adapter = super::enumerate_native_adapters()?
        .into_iter()
        .find(|adapter| adapter.vendor_id == 0x10DE)
        .ok_or("no NVIDIA adapter for the shared-texture proof")?;
    let device = super::native::qualification_device_for_luid(adapter.luid)?;
    let (texture, shared) = create_shared(&device)?;
    let mutex: IDXGIKeyedMutex = texture.cast()?;
    let (mut child, ready, _done) = child_process("stall", adapter.luid, shared.0)?;
    let started = Instant::now();
    // The ready wait consumes at most 5 s; the whole post-setup transaction,
    // including kill, exit confirmation and mutex inspection, has 7 s.
    let deadline = started + Duration::from_secs(7);
    let wait = wait_ready_or_exit(&child, ready.0);
    let wait_elapsed = started.elapsed();
    if !wait
        .as_ref()
        .is_err_and(|error| error.to_string().contains("timed out"))
    {
        let cleanup = stop_child_before(&mut child, deadline);
        return Err(format!("expected Ready timeout, got {wait:?}; cleanup: {cleanup:?}").into());
    }
    let kill_requested_at = started.elapsed();
    stop_child_before(&mut child, deadline)?;
    let process_signaled_at = started.elapsed();
    let abandoned = match mutex_acquire(&mutex, 1, remaining_ms(deadline).min(100)) {
        Ok(()) => return Err("stalled child unexpectedly transferred mutex ownership".into()),
        Err(error) => error.to_string(),
    };
    if !abandoned.contains("WAIT_ABANDONED") {
        return Err(format!("stalled child mutex state: {abandoned}").into());
    }
    let source_lost = RecorderError {
        code: ErrorCode::SourceLost,
        message: "Shared-texture child stopped while holding an unpublished slot.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new capture session.".into(),
    };
    if source_lost.code != ErrorCode::SourceLost {
        return Err("watchdog failure was not SourceLost".into());
    }
    Ok(format!(
        "watchdog=SourceLost ready_wait_ms={} kill_request_ms={} process_signal_ms={} cleanup_after_kill_ms={} total_ms={} mutex=WAIT_ABANDONED published=0 slots_reused=0 deadline_ms=7000",
        wait_elapsed.as_millis(),
        kill_requested_at.as_millis(),
        process_signaled_at.as_millis(),
        (process_signaled_at - kill_requested_at).as_millis(),
        started.elapsed().as_millis(),
    ))
}
