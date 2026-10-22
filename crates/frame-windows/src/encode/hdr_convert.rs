//! Same-device, GPU-only conversion from linear scRGB capture to PQ BT.2020 RGB10.
//! The NVENC bridge registers only the converted R10G10B10A2 texture. This module
//! never maps a capture frame to the CPU or labels linear scRGB as PQ.

use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Duration, Instant};

use frame_contracts::RecorderError;
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::core::{BOOL, Interface};

use super::{internal_error, preparation_error};

// scRGB is linear BT.709/D65 with 1.0 representing 80 cd/m². The 3x3 is the
// linear BT.709 to BT.2020 matrix. Values outside the representable BT.2020/PQ
// gamut are clipped explicitly, never silently tone-mapped.
const SHADER: &[u8] = br#"
Texture2D<float4> sourceTexture : register(t0);
struct VertexOut { float4 position : SV_POSITION; };
VertexOut vs(uint vertex : SV_VertexID) {
    VertexOut output;
    float2 positions[3] = { float2(-1,-1), float2(-1,3), float2(3,-1) };
    output.position = float4(positions[vertex], 0, 1);
    return output;
}
float3 pq(float3 value) {
    float3 l = pow(saturate(value * 0.008), 0.1593017578125);
    return pow((0.8359375 + 18.8515625 * l) / (1.0 + 18.6875 * l), 78.84375);
}
float4 ps(VertexOut input) : SV_Target {
    float3 rgb = sourceTexture.Load(int3(int2(input.position.xy), 0)).rgb;
    float3 bt2020 = float3(
        dot(rgb, float3(0.6274040, 0.3292820, 0.0433136)),
        dot(rgb, float3(0.0690970, 0.9195400, 0.0113612)),
        dot(rgb, float3(0.0163916, 0.0880132, 0.8955950)));
    return float4(pq(bt2020), 1.0);
}
"#;

fn native(api: &str, error: windows::core::Error) -> RecorderError {
    internal_error(format!(
        "HDR GPU conversion {api} failed with HRESULT 0x{:08X}.",
        error.code().0
    ))
}

fn compile(entry: &'static [u8], target: &'static [u8]) -> Result<Vec<u8>, RecorderError> {
    let mut code: Option<ID3DBlob> = None;
    unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
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
    .map_err(|error| native("shader compilation", error))?;
    let code = code.ok_or_else(|| preparation_error("HDR shader returned no bytecode.".into()))?;
    Ok(
        unsafe { std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize()) }
            .to_vec(),
    )
}

pub(super) struct HdrInput {
    pub(super) target: ID3D11Texture2D,
    source_view: ID3D11ShaderResourceView,
    target_view: ID3D11RenderTargetView,
    complete: ID3D11Query,
}

pub(super) struct HdrConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    inputs: Vec<Option<HdrInput>>,
}

impl HdrConverter {
    pub(super) fn new(device: *mut c_void) -> Result<Self, RecorderError> {
        let device = unsafe { ID3D11Device::from_raw_borrowed(&device) }
            .ok_or_else(|| preparation_error("HDR capture device is unavailable.".into()))?
            .clone();
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|error| native("get immediate context", error))?;
        let vertex_code = compile(b"vs\0", b"vs_4_0\0")?;
        let pixel_code = compile(b"ps\0", b"ps_4_0\0")?;
        let mut vertex = None;
        let mut pixel = None;
        unsafe {
            device
                .CreateVertexShader(&vertex_code, None::<&ID3D11ClassLinkage>, Some(&mut vertex))
                .map_err(|error| native("create vertex shader", error))?;
            device
                .CreatePixelShader(&pixel_code, None::<&ID3D11ClassLinkage>, Some(&mut pixel))
                .map_err(|error| native("create pixel shader", error))?;
        }
        Ok(Self {
            device,
            context,
            vertex: vertex
                .ok_or_else(|| preparation_error("HDR vertex shader is absent.".into()))?,
            pixel: pixel.ok_or_else(|| preparation_error("HDR pixel shader is absent.".into()))?,
            inputs: (0..4).map(|_| None).collect(),
        })
    }

    pub(super) fn prepare_input(
        &self,
        source: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<HdrInput, RecorderError> {
        let source = unsafe { ID3D11Texture2D::from_raw_borrowed(&source) }
            .ok_or_else(|| preparation_error("HDR source texture is unavailable.".into()))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT
            || desc.Width != width
            || desc.Height != height
            || desc.SampleDesc.Count != 1
            || desc.MipLevels != 1
            || desc.BindFlags & (D3D11_BIND_SHADER_RESOURCE.0 as u32) == 0
        {
            return Err(preparation_error(
                "HDR input must be a same-size, single-sample FP16 scRGB shader resource.".into(),
            ));
        }
        let mut source_view = None;
        unsafe {
            self.device
                .CreateShaderResourceView(source, None, Some(&mut source_view))
        }
        .map_err(|error| native("create FP16 source view", error))?;
        let target_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut target = None;
        unsafe {
            self.device
                .CreateTexture2D(&target_desc, None, Some(&mut target))
        }
        .map_err(|error| native("create RGB10 encode texture", error))?;
        let target =
            target.ok_or_else(|| preparation_error("HDR RGB10 texture is absent.".into()))?;
        let mut target_view = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&target, None, Some(&mut target_view))
        }
        .map_err(|error| native("create RGB10 target view", error))?;
        let mut complete = None;
        unsafe {
            self.device.CreateQuery(
                &D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                },
                Some(&mut complete),
            )
        }
        .map_err(|error| native("create GPU completion query", error))?;
        Ok(HdrInput {
            target,
            source_view: source_view
                .ok_or_else(|| preparation_error("HDR source view is absent.".into()))?,
            target_view: target_view
                .ok_or_else(|| preparation_error("HDR target view is absent.".into()))?,
            complete: complete
                .ok_or_else(|| preparation_error("HDR completion query is absent.".into()))?,
        })
    }

    pub(super) fn retain(
        &mut self,
        registration: u32,
        input: HdrInput,
    ) -> Result<(), RecorderError> {
        let slot = self.inputs.get_mut(registration as usize).ok_or_else(|| {
            internal_error(format!(
                "NVENC returned out-of-range HDR registration {registration}."
            ))
        })?;
        *slot = Some(input);
        Ok(())
    }

    pub(super) fn release(&mut self, registration: u32) {
        if let Some(slot) = self.inputs.get_mut(registration as usize) {
            *slot = None;
        }
    }

    pub(super) fn convert(&self, registration: u32) -> Result<(), RecorderError> {
        let input = self
            .inputs
            .get(registration as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                internal_error(format!(
                    "HDR registration {registration} has no GPU conversion input."
                ))
            })?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input.target.GetDesc(&mut desc) };
        let multi: ID3D11Multithread = self
            .context
            .cast()
            .map_err(|error| native("lock shared context", error))?;
        unsafe { multi.Enter() };
        let result = (|| {
            unsafe {
                self.context.OMSetRenderTargets(
                    Some(&[Some(input.target_view.clone())]),
                    None::<&ID3D11DepthStencilView>,
                );
                self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: desc.Width as f32,
                    Height: desc.Height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
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
                    .PSSetShaderResources(0, Some(&[Some(input.source_view.clone())]));
                self.context.Draw(3, 0);
                self.context.PSSetShaderResources(0, Some(&[None]));
                self.context
                    .OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
                self.context.End(&input.complete);
                self.context.Flush();
            }
            let started = Instant::now();
            loop {
                let mut complete = BOOL(0);
                let result = unsafe {
                    self.context.GetData(
                        &input.complete,
                        Some((&mut complete as *mut BOOL).cast()),
                        size_of::<BOOL>() as u32,
                        D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                    )
                };
                match result {
                    Ok(()) if complete.as_bool() => return Ok(()),
                    Ok(()) => {}
                    Err(error) => return Err(native("wait for RGB10 conversion", error)),
                }
                if started.elapsed() >= Duration::from_secs(5) {
                    return Err(internal_error(
                        "HDR GPU conversion exceeded its five-second watchdog.".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })();
        unsafe { multi.Leave() };
        result
    }
}
