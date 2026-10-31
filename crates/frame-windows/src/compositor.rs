//! Same-device D3D11 overlays over the desktop. Camera uploads and captured window
//! textures share the existing viewport draw, bounded output pool and completion fence.
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::time::{Duration, Instant};

use frame_capture::{CaptureSession, CapturedFrame, PipelineWake};
use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{CameraPlacement, CameraSelection, ErrorCode, RecorderError, RecoveryStatus};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::{BOOL, Interface};

use crate::camera::{CameraFrame, CameraSession};
use crate::preview::{PreviewHub, PreviewSource, ThumbnailGenerator, error_image};
use crate::surface::{
    EncodeTexture, MAX_PAYLOAD_BYTES, PayloadOwner, SharedPayload, SurfaceIdentity, SurfacePool,
    WindowsSurface, plan_owned_payload,
};

pub(crate) mod fit;

const SHADER: &[u8] = br#"
Texture2D cameraTexture : register(t0);
SamplerState cameraSampler : register(s0);
struct VertexOut { float4 position : SV_POSITION; float2 uv : TEXCOORD0; };
VertexOut vs(uint vertex : SV_VertexID) {
  VertexOut output;
  float2 positions[3] = { float2(-1,-1), float2(-1,3), float2(3,-1) };
  float2 uv[3] = { float2(0,1), float2(0,-1), float2(2,1) };
  output.position = float4(positions[vertex], 0, 1);
  output.uv = uv[vertex];
  return output;
}
float4 ps(VertexOut input) : SV_Target { return cameraTexture.Sample(cameraSampler, input.uv); }
"#;

fn fail(message: impl Into<String>) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Refresh the selected display and overlay sources; choose supported SDR formats and retry."
            .into(),
    }
}
fn native(api: &str, error: windows::core::Error) -> RecorderError {
    fail(format!(
        "Overlay compositor {api} failed with HRESULT 0x{:08X}.",
        error.code().0
    ))
}

pub fn validate_placement(
    place: CameraPlacement,
    width: u32,
    height: u32,
) -> Result<(), RecorderError> {
    if place.width == 0
        || place.height == 0
        || place
            .x
            .checked_add(place.width)
            .is_none_or(|end| end > width)
        || place
            .y
            .checked_add(place.height)
            .is_none_or(|end| end > height)
    {
        return Err(RecorderError::invalid(
            "Overlay placement must be nonempty and entirely inside the selected display or region.",
        ));
    }
    Ok(())
}

struct Backing {
    _source: SharedPayload,
    device: usize,
    textures: Vec<ID3D11Texture2D>,
    pool: u64,
    width: u32,
    height: u32,
}
impl PayloadOwner for Backing {
    fn encode_texture(&self, index: usize) -> Option<EncodeTexture> {
        let texture = self.textures.get(index)?;
        Some(EncodeTexture {
            device: self.device as *mut c_void,
            texture: texture.as_raw(),
            identity: SurfaceIdentity {
                pool: self.pool,
                device: self.device,
                texture: texture.as_raw() as usize,
                width: self.width,
                height: self.height,
            },
        })
    }
}

struct GpuCompositor {
    context: ID3D11DeviceContext,
    backing: Arc<Backing>,
    pool: Arc<SurfacePool>,
    targets: Vec<ID3D11RenderTargetView>,
    queries: Vec<ID3D11Query>,
    inputs: Vec<GpuInput>,
    sampler: ID3D11SamplerState,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    preview: Option<ComposedPreview>,
    last_composed: Option<(usize, u64)>,
}

enum OverlayFrame<'a> {
    Camera(&'a CameraFrame, CameraPlacement),
}
impl OverlayFrame<'_> {
    fn geometry(&self) -> (u32, u32, bool) {
        match self {
            Self::Camera(frame, _) => (frame.width, frame.height, false),
        }
    }
    fn placement(&self) -> CameraPlacement {
        match self {
            Self::Camera(_, p) => *p,
        }
    }
}

struct GpuInput {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    size: (u32, u32),
    uploaded_camera_timestamp: Option<MediaTimestamp>,
}

/// Includes the desktop pool, composed pool and sampled camera input textures.
fn composition_bytes(
    width: u32,
    height: u32,
    inputs: &[(u32, u32, bool)],
) -> Result<u64, RecorderError> {
    let mut bytes = plan_owned_payload(width, height, false)?
        .slots_bytes
        .checked_mul(2);
    for &(w, h, window) in inputs {
        let scratch = u64::from(w)
            .checked_mul(u64::from(h))
            .and_then(|v| v.checked_mul(4));
        let source = if window {
            plan_owned_payload(w, h, false)?.slots_bytes
        } else {
            0
        };
        bytes = bytes
            .and_then(|v| v.checked_add(source))
            .and_then(|v| scratch.and_then(|n| v.checked_add(n)));
    }
    bytes.filter(|v| *v <= MAX_PAYLOAD_BYTES).ok_or_else(|| {
        RecorderError::unsupported(
            "The selected overlay sources exceed the bounded GPU composition budget.",
        )
    })
}

struct ComposedPreview {
    device: ID3D11Device,
    gpu_image: Result<Arc<crate::gpu_preview::SharedImage>, String>,
    owned_bytes: u64,
    hub: Arc<PreviewHub>,
    generator: ThumbnailGenerator,
    context: ID3D11DeviceContext,
    scratch: Vec<u8>,
    last_queued: Option<Instant>,
    last_media: Option<u64>,
    last_epoch: u64,
}
impl ComposedPreview {
    fn poll(&mut self) {
        if !self.hub.enabled() || self.hub.gpu_sink().is_some() || !self.hub.cpu_preview_allowed() {
            self.generator.discard_pending();
            return;
        }
        if !self.generator.has_pending() {
            return;
        }
        let result = ContextGuard::enter(&self.context)
            .map_err(|error| error.message)
            .and_then(|_guard| {
                self.generator
                    .complete_pending(&mut self.scratch)
                    .map_err(|error| error.message().to_string())
            });
        match result {
            Ok(Some((image, _cost))) => {
                let _ = self.hub.publish(image);
            }
            Ok(None) => {}
            Err(message) => {
                self.generator.discard_pending();
                self.hub.publish_error(error_image(0, 0, None, message));
            }
        }
    }
    fn queue(&mut self, texture: &ID3D11Texture2D, width: u32, height: u32, media_ns: u64) {
        self.poll();
        let epoch = self.hub.subscription_epoch();
        if epoch != self.last_epoch {
            self.last_epoch = epoch;
            self.last_media = None;
        }
        if !self.hub.enabled() {
            self.last_media = None;
            self.generator.discard_pending();
            self.hub.note_skipped();
            return;
        }
        if let Some(gpu) = self.hub.gpu_sink() {
            self.generator.discard_pending();
            if self.last_media == Some(media_ns) || !gpu.admit_frame() {
                return;
            }
            match &self.gpu_image {
                Ok(image) if gpu.prepare_image(image) => {}
                Ok(_) => return,
                Err(error) => {
                    gpu.fail(error.clone());
                    return;
                }
            }
            let result = ContextGuard::enter(&self.context)
                .map_err(|e| e.message)
                .and_then(|_guard| {
                    gpu.submit(
                        &PreviewSource::new(texture, width, height),
                        &self.device,
                        &self.context,
                        self.owned_bytes,
                    )
                });
            match result {
                Ok(()) => self.last_media = Some(media_ns),
                Err(error) => gpu.fail(error),
            }
            return;
        }
        if !self.hub.cpu_preview_allowed() {
            return;
        }
        if self.hub.subscriber_count() == 0 {
            self.last_media = None;
            self.hub.note_skipped();
            return;
        }
        if self.last_media == Some(media_ns) {
            return;
        }
        if self
            .last_queued
            .is_some_and(|when| when.elapsed() < self.hub.config().min_interval)
        {
            self.hub.note_rate_limited();
            return;
        }
        let result = ContextGuard::enter(&self.context)
            .map_err(|error| error.message)
            .and_then(|_guard| {
                self.generator
                    .queue(PreviewSource::new(texture, width, height), Some(media_ns))
                    .map_err(|error| error.message().to_string())
            });
        match result {
            Ok(true) => {
                self.last_queued = Some(Instant::now());
                self.last_media = Some(media_ns);
            }
            Ok(false) => {}
            Err(message) => {
                self.last_queued = Some(Instant::now());
                self.last_media = Some(media_ns);
                self.hub
                    .publish_error(error_image(0, 0, Some(media_ns), message));
            }
        }
    }
}

struct ContextGuard(ID3D11Multithread);
impl ContextGuard {
    fn enter(context: &ID3D11DeviceContext) -> Result<Self, RecorderError> {
        let multi: ID3D11Multithread = context
            .cast()
            .map_err(|e| native("lock shared context", e))?;
        unsafe { multi.Enter() };
        Ok(Self(multi))
    }
}
impl Drop for ContextGuard {
    fn drop(&mut self) {
        unsafe { self.0.Leave() };
    }
}

pub(crate) fn compile(
    source: &[u8],
    entry: &'static [u8],
    target: &'static [u8],
) -> Result<Vec<u8>, RecorderError> {
    let mut code: Option<ID3DBlob> = None;
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::PCSTR::null(),
            None,
            None::<&windows::Win32::Graphics::Direct3D::ID3DInclude>,
            windows::core::PCSTR(entry.as_ptr()),
            windows::core::PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            None,
        )
    }
    .map_err(|e| native("compile shader", e))?;
    let code = code.ok_or_else(|| fail("Camera shader compilation returned no bytecode."))?;
    Ok(
        unsafe { std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize()) }
            .to_vec(),
    )
}

impl GpuCompositor {
    fn new(
        screen: &WindowsSurface,
        width: u32,
        height: u32,
        overlays: &[OverlayFrame<'_>],
        preview: Option<Arc<PreviewHub>>,
    ) -> Result<Self, RecorderError> {
        for overlay in overlays {
            validate_placement(overlay.placement(), width, height)?;
        }
        let input_sizes: Vec<_> = overlays.iter().map(OverlayFrame::geometry).collect();
        let owned_bytes = composition_bytes(width, height, &input_sizes)?;
        let view = screen
            .encode_texture()
            .ok_or_else(|| fail("Display surface has no D3D11 texture."))?;
        if view.identity.width != width || view.identity.height != height {
            return Err(fail("Display geometry changed before camera composition."));
        }
        let source_texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&view.texture) }
            .ok_or_else(|| fail("Display texture is unavailable."))?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source_texture.GetDesc(&mut source_desc) };
        if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || source_desc.Width != width
            || source_desc.Height != height
            || source_desc.SampleDesc.Count != 1
            || source_desc.MipLevels != 1
        {
            return Err(fail(
                "Display texture format is not single-sample BGRA8 at the selected geometry.",
            ));
        }
        let plan = plan_owned_payload(width, height, false)?;
        let pool = SurfacePool::new(plan.slots_bytes);
        let device = unsafe { ID3D11Device::from_raw_borrowed(&view.device) }
            .ok_or_else(|| fail("Display device is unavailable."))?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| native("get display context", e))?;
        let desc = D3D11_TEXTURE2D_DESC {
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
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut textures = Vec::with_capacity(4);
        let mut targets = Vec::with_capacity(4);
        let mut queries = Vec::with_capacity(4);
        for _ in 0..4 {
            let mut texture = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(|e| native("create composed texture", e))?;
            let texture = texture.ok_or_else(|| fail("D3D11 returned no composed texture."))?;
            let mut target = None;
            unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut target)) }
                .map_err(|e| native("create render target", e))?;
            let mut query = None;
            unsafe {
                device.CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
            }
            .map_err(|e| native("create completion query", e))?;
            textures.push(texture);
            targets.push(target.ok_or_else(|| fail("D3D11 returned no render target."))?);
            queries.push(query.ok_or_else(|| fail("D3D11 returned no completion query."))?);
        }
        let mut inputs = Vec::with_capacity(overlays.len());
        for &(width, height, _) in &input_sizes {
            let input_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..desc
            };
            let mut texture = None;
            unsafe { device.CreateTexture2D(&input_desc, None, Some(&mut texture)) }
                .map_err(|e| native("create overlay texture", e))?;
            let texture = texture.ok_or_else(|| fail("D3D11 returned no overlay texture."))?;
            let mut view = None;
            unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut view)) }
                .map_err(|e| native("create overlay shader view", e))?;
            inputs.push(GpuInput {
                texture,
                view: view.ok_or_else(|| fail("D3D11 returned no overlay shader view."))?,
                size: (width, height),
                uploaded_camera_timestamp: None,
            });
        }
        let vertex_code = compile(SHADER, b"vs\0", b"vs_4_0\0")?;
        let pixel_code = compile(SHADER, b"ps\0", b"ps_4_0\0")?;
        let mut vertex = None;
        let mut pixel = None;
        unsafe {
            device
                .CreateVertexShader(&vertex_code, None::<&ID3D11ClassLinkage>, Some(&mut vertex))
                .map_err(|e| native("create vertex shader", e))?;
            device
                .CreatePixelShader(&pixel_code, None::<&ID3D11ClassLinkage>, Some(&mut pixel))
                .map_err(|e| native("create pixel shader", e))?;
        }
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .map_err(|e| native("create overlay sampler", e))?;
        let backing = Arc::new(Backing {
            _source: screen.encode_backing(),
            device: view.identity.device,
            textures,
            pool: pool.id(),
            width,
            height,
        });
        let preview = preview
            .map(|hub| {
                ThumbnailGenerator::create(
                    Some(device.clone()),
                    Some(context.clone()),
                    hub.config(),
                    owned_bytes,
                    hub.counters_handle(),
                )
                .map(|generator| ComposedPreview {
                    device: device.clone(),
                    gpu_image: crate::gpu_preview::prepare_shared(
                        device,
                        width,
                        height,
                        owned_bytes,
                    ),
                    owned_bytes,
                    hub,
                    generator,
                    context: context.clone(),
                    scratch: Vec::new(),
                    last_queued: None,
                    last_media: None,
                    last_epoch: 0,
                })
            })
            .transpose()
            .map_err(|e| fail(format!("Composed preview initialization failed: {e}")))?;
        Ok(Self {
            context,
            backing,
            pool,
            targets,
            queries,
            inputs,
            sampler: sampler.ok_or_else(|| fail("D3D11 returned no overlay sampler."))?,
            vertex: vertex.ok_or_else(|| fail("D3D11 returned no vertex shader."))?,
            pixel: pixel.ok_or_else(|| fail("D3D11 returned no pixel shader."))?,
            preview,
            last_composed: None,
        })
    }

    fn preview_poll(&mut self) {
        if let Some(preview) = self.preview.as_mut() {
            preview.poll();
            if let Some((index, media_ns)) = self.last_composed {
                preview.queue(
                    &self.backing.textures[index],
                    self.backing.width,
                    self.backing.height,
                    media_ns,
                );
            }
        }
    }

    fn compose(
        &mut self,
        screen: &WindowsSurface,
        overlays: &[OverlayFrame<'_>],
        media_ns: u64,
    ) -> Result<WindowsSurface, RecorderError> {
        let view = screen
            .encode_texture()
            .ok_or_else(|| fail("Display surface lost its D3D11 texture."))?;
        if view.identity.device != self.backing.device
            || view.identity.width != self.backing.width
            || view.identity.height != self.backing.height
        {
            return Err(fail(
                "Display device or geometry changed; cross-device composition is unsupported.",
            ));
        }
        if overlays.len() != self.inputs.len() {
            return Err(fail("The overlay set changed during recording."));
        }
        let source = unsafe { ID3D11Texture2D::from_raw_borrowed(&view.texture) }
            .ok_or_else(|| fail("Display texture is unavailable."))?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || source_desc.Width != self.backing.width
            || source_desc.Height != self.backing.height
            || source_desc.SampleDesc.Count != 1
            || source_desc.MipLevels != 1
        {
            return Err(fail(
                "Display texture format or geometry changed during overlay composition.",
            ));
        }
        // Preflight every input before submitting any GPU work. Never sample a
        // window from another device or release a source lease before completion.
        for (input, overlay) in self.inputs.iter().zip(overlays) {
            let (w, h, _) = overlay.geometry();
            validate_placement(overlay.placement(), self.backing.width, self.backing.height)?;
            if input.size != (w, h) {
                return Err(fail("Overlay geometry changed during recording."));
            }
            match overlay {
                OverlayFrame::Camera(frame, _) => {
                    if frame.bgra.len() as u64 != u64::from(w) * u64::from(h) * 4 {
                        return Err(fail("Camera frame payload does not match its format."));
                    }
                }
            }
        }
        let context_guard = ContextGuard::enter(&self.context)?;
        let slot = self
            .pool
            .try_acquire_copy_slot()
            .ok_or_else(|| fail("All four composed texture leases remain held."))?;
        let index = slot.index();
        let target = &self.backing.textures[index];
        unsafe {
            self.context.CopyResource(target, source);
            self.context.OMSetRenderTargets(
                Some(&[Some(self.targets[index].clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            self.context.RSSetState(None::<&ID3D11RasterizerState>);
            self.context
                .OMSetBlendState(None::<&ID3D11BlendState>, None, u32::MAX);
            self.context
                .OMSetDepthStencilState(None::<&ID3D11DepthStencilState>, 0);
            self.context.IASetInputLayout(None::<&ID3D11InputLayout>);
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            // List order is back-to-front. The camera, when selected, is last.
            for (input, overlay) in self.inputs.iter_mut().zip(overlays) {
                match overlay {
                    OverlayFrame::Camera(frame, _) => {
                        if input.uploaded_camera_timestamp != Some(frame.timestamp) {
                            self.context.UpdateSubresource(
                                &input.texture,
                                0,
                                None,
                                frame.bgra.as_ptr().cast(),
                                frame.width * 4,
                                0,
                            );
                            input.uploaded_camera_timestamp = Some(frame.timestamp);
                        }
                    }
                }
                let place = overlay.placement();
                self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: place.x as f32,
                    TopLeftY: place.y as f32,
                    Width: place.width as f32,
                    Height: place.height as f32,
                    MinDepth: 0.,
                    MaxDepth: 1.,
                }]));
                self.context
                    .PSSetShaderResources(0, Some(&[Some(input.view.clone())]));
                self.context.Draw(3, 0);
                self.context.PSSetShaderResources(0, Some(&[None]));
            }
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context
                .OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
            self.context.End(&self.queries[index]);
            self.context.Flush();
        }
        drop(context_guard);
        let started = Instant::now();
        loop {
            let mut complete = BOOL(0);
            let result = unsafe {
                self.context.GetData(
                    &self.queries[index],
                    Some((&mut complete as *mut BOOL).cast()),
                    size_of::<BOOL>() as u32,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                )
            };
            match result {
                Ok(()) if complete.as_bool() => break,
                Ok(()) => {}
                Err(e) => {
                    slot.mark_free();
                    return Err(native("observe GPU composition", e));
                }
            }
            if started.elapsed() >= Duration::from_secs(5) {
                slot.mark_free();
                return Err(fail(
                    "GPU overlay composition exceeded its five-second completion watchdog.",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        slot.mark_ready();
        slot.mark_consumer_owned();
        self.last_composed = Some((index, media_ns));
        if let Some(preview) = self.preview.as_mut() {
            preview.queue(target, self.backing.width, self.backing.height, media_ns);
        }
        // The completion query covers every source read. The cached source
        // leases remain owned by the session; consumers own only this output.
        Ok(WindowsSurface::new(
            slot,
            Arc::clone(&self.backing) as SharedPayload,
        ))
    }
}

/// Capture adapter for explicit camera/window overlays. The desktop remains the
/// fixed canvas; every source shares its clock and the desktop's D3D11 device.
pub struct CameraCaptureSession {
    screen: Box<dyn CaptureSession<Surface = WindowsSurface>>,
    camera: Option<CameraSession>,
    latest_camera: Option<CameraFrame>,
    latest_screen: Option<CapturedFrame<WindowsSurface>>,
    gpu: Option<GpuCompositor>,
    placement: Option<CameraPlacement>,
    preview: Option<Arc<PreviewHub>>,
    clock: Arc<MasterClock>,
    last_output: Option<MediaTimestamp>,
    stopped: bool,
    canvas: (u32, u32),
}

impl CameraCaptureSession {
    /// Idle placement edits reuse capture sessions and their shared device.
    /// Source identity/order changes are handled by the control owner's restart.
    pub fn update_placement(
        &mut self,
        camera: Option<CameraPlacement>,
    ) -> Result<(), RecorderError> {
        if self.placement.is_some() != camera.is_some() {
            return Err(RecorderError::invalid(
                "Placement edits cannot change camera sources.",
            ));
        }
        if let Some(place) = camera {
            validate_placement(place, self.canvas.0, self.canvas.1)?;
        }
        self.placement = camera;
        self.last_output = None;
        Ok(())
    }
    pub fn start(
        screen: Box<dyn CaptureSession<Surface = WindowsSurface>>,
        camera: Option<&CameraSelection>,
        clock: Arc<MasterClock>,
        width: u32,
        height: u32,
        preview: Option<Arc<PreviewHub>>,
    ) -> Result<Self, RecorderError> {
        // Construct the teardown owner before opening any other source.
        let mut session = Self {
            screen,
            camera: None,
            latest_camera: None,
            latest_screen: None,
            gpu: None,
            placement: camera.map(|c| c.placement),
            preview,
            clock: Arc::clone(&clock),
            last_output: None,
            stopped: false,
            canvas: (width, height),
        };
        let mut sizes = Vec::new();
        if let Some(choice) = camera {
            validate_placement(choice.placement, width, height)?;
            match CameraSession::start(choice, Arc::clone(&clock))
                .and_then(|camera| camera.wait_first_frame().map(|first| (camera, first)))
            {
                Ok((camera, first)) => {
                    sizes.push((first.width, first.height, false));
                    session.latest_camera = Some(first);
                    session.camera = Some(camera);
                }
                Err(error) => {
                    if let Some(hub) = &session.preview {
                        hub.report_camera_warning(error.message);
                    }
                    // The optional device cannot take ownership of the desktop failure path.
                }
            }
        }
        composition_bytes(width, height, &sizes)?;
        Ok(session)
    }
}
impl CaptureSession for CameraCaptureSession {
    type Surface = WindowsSurface;
    fn set_wake_signal(&mut self, wake: PipelineWake) {
        self.screen.set_wake_signal(wake.clone());
    }
    fn set_replay_sampling(&mut self, enabled: bool) {
        self.screen.set_replay_sampling(enabled);
    }
    fn pipeline_stats(&self) -> Option<frame_capture::CapturePipelineStats> {
        // These remain desktop counters, not a claim about distinct scene frames.
        self.screen.pipeline_stats()
    }
    fn try_frame(&mut self) -> Result<Option<CapturedFrame<WindowsSurface>>, RecorderError> {
        if self.stopped {
            return Ok(None);
        }
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.preview_poll();
        }
        let mut changed = self.last_output.is_none();
        if let Some(camera) = self.camera.as_mut() {
            match camera.try_latest() {
                Ok(Some(frame)) => {
                    self.latest_camera = Some(frame);
                    changed = true;
                }
                Ok(None) => (),
                Err(error) => {
                    if let Some(hub) = &self.preview {
                        hub.report_camera_warning(error.message);
                    }
                    self.camera = None;
                    self.latest_camera = None;
                    self.gpu = None;
                    changed = true;
                }
            }
        }
        if let Some(frame) = self.screen.try_frame()? {
            self.latest_screen = Some(frame);
            changed = true;
        }
        let Some(screen) = &self.latest_screen else {
            return Ok(None);
        };

        if !changed {
            return Ok(None);
        }
        let newest_source = self
            .latest_camera
            .iter()
            .map(|f| f.timestamp)
            .chain(std::iter::once(screen.timestamp))
            .max()
            .expect("desktop timestamp");
        // A scene sample is made only after a real source update. Timestamp it
        // on the shared clock when composed: a late WGC update must still appear
        // over a static desktop whose last native timestamp was newer.
        let timestamp = self.clock.now().max(newest_source);
        if self.last_output.is_some_and(|last| timestamp <= last) {
            return Ok(None);
        }
        let mut overlays = Vec::new();
        if let (Some(camera), Some(place)) = (&self.latest_camera, self.placement) {
            overlays.push(OverlayFrame::Camera(camera, place));
        }
        if self.gpu.is_none() {
            self.gpu = Some(GpuCompositor::new(
                &screen.surface,
                screen.width,
                screen.height,
                &overlays,
                self.preview.clone(),
            )?);
        }
        let surface = self.gpu.as_mut().expect("created compositor").compose(
            &screen.surface,
            &overlays,
            timestamp.0,
        )?;
        self.last_output = Some(timestamp);
        Ok(Some(CapturedFrame {
            surface,
            timestamp,
            width: screen.width,
            height: screen.height,
        }))
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if let Some(preview) = self.preview.take() {
            preview.close();
        }
        self.gpu = None;
        self.latest_screen = None;
        self.latest_camera = None;
        let mut result = Ok(());

        if let Some(camera) = &mut self.camera {
            result = result.and(camera.stop());
        }
        result.and(self.screen.stop())
    }
}
impl Drop for CameraCaptureSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placement_is_strict() {
        assert!(
            validate_placement(
                CameraPlacement {
                    x: 80,
                    y: 20,
                    width: 20,
                    height: 30
                },
                100,
                50
            )
            .is_ok()
        );
        assert!(
            validate_placement(
                CameraPlacement {
                    x: 81,
                    y: 20,
                    width: 20,
                    height: 30
                },
                100,
                50
            )
            .is_err()
        );
        assert!(
            validate_placement(
                CameraPlacement {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 30
                },
                100,
                50
            )
            .is_err()
        );
    }
}
