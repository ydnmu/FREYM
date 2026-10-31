//! GPU-only aspect fit into an existing, fixed recording canvas. The camera
//! compositor's texture shader is shared; ordinary 1:1 window copies bypass this.
use super::{SHADER, compile};
use frame_contracts::RecorderError;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

pub(crate) struct TextureFit {
    device: ID3D11Device,
    targets: Vec<ID3D11RenderTargetView>,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    input: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, (u32, u32))>,
    canvas: (u32, u32),
}

fn error(api: &str, e: windows::core::Error) -> RecorderError {
    RecorderError::unsupported(&format!("Window resize GPU fit {api} failed: {e}"))
}

impl TextureFit {
    pub(crate) fn input_size(&self) -> Option<(u32, u32)> {
        self.input.as_ref().map(|(_, _, size)| *size)
    }

    pub(crate) fn new(
        device: &ID3D11Device,
        textures: &[ID3D11Texture2D],
        canvas: (u32, u32),
    ) -> Result<Self, RecorderError> {
        let shader_error = |error: RecorderError| {
            RecorderError::unsupported(&format!(
                "Window resize GPU fit shader initialization failed: {}",
                error.message
            ))
        };
        let vertex_code = compile(SHADER, b"vs\0", b"vs_4_0\0").map_err(shader_error)?;
        let pixel_code = compile(SHADER, b"ps\0", b"ps_4_0\0").map_err(shader_error)?;
        let mut vertex = None;
        let mut pixel = None;
        let mut sampler = None;
        let mut targets = Vec::with_capacity(textures.len());
        unsafe {
            device
                .CreateVertexShader(&vertex_code, None::<&ID3D11ClassLinkage>, Some(&mut vertex))
                .map_err(|e| error("CreateVertexShader", e))?;
            device
                .CreatePixelShader(&pixel_code, None::<&ID3D11ClassLinkage>, Some(&mut pixel))
                .map_err(|e| error("CreatePixelShader", e))?;
            device
                .CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut sampler),
                )
                .map_err(|e| error("CreateSamplerState", e))?;
            for texture in textures {
                let mut target = None;
                device
                    .CreateRenderTargetView(texture, None, Some(&mut target))
                    .map_err(|e| error("CreateRenderTargetView", e))?;
                targets.push(target.ok_or_else(|| {
                    RecorderError::unsupported("Window fit has no render target.")
                })?);
            }
        }
        Ok(Self {
            device: device.clone(),
            targets,
            vertex: vertex
                .ok_or_else(|| RecorderError::unsupported("Window fit has no vertex shader."))?,
            pixel: pixel
                .ok_or_else(|| RecorderError::unsupported("Window fit has no pixel shader."))?,
            sampler: sampler
                .ok_or_else(|| RecorderError::unsupported("Window fit has no sampler."))?,
            input: None,
            canvas,
        })
    }

    /// Allocates once per source geometry. Drop the old input before allocation,
    /// and include this extra BGRA surface in the existing capture payload gate.
    pub(crate) fn prepare(&mut self, size: (u32, u32)) -> Result<(), RecorderError> {
        if self
            .input
            .as_ref()
            .is_some_and(|(_, _, current)| *current == size)
        {
            return Ok(());
        }
        let owned = crate::surface::plan_owned_payload(self.canvas.0, self.canvas.1, false)?;
        let scratch = u64::from(size.0)
            .checked_mul(u64::from(size.1))
            .and_then(|v| v.checked_mul(4));
        if size.0 == 0
            || size.1 == 0
            || scratch
                .and_then(|v| v.checked_add(owned.total_bytes))
                .is_none_or(|v| v > crate::surface::MAX_PAYLOAD_BYTES)
        {
            return Err(RecorderError::unsupported(
                "Resized window exceeds the GPU capture payload budget.",
            ));
        }
        self.input = None;
        let mut texture = None;
        let mut view = None;
        unsafe {
            self.device
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: size.0,
                        Height: size.1,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )
                .map_err(|e| error("CreateTexture2D", e))?;
            let texture = texture
                .ok_or_else(|| RecorderError::unsupported("Window fit has no input texture."))?;
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
                .map_err(|e| error("CreateShaderResourceView", e))?;
            self.input = Some((
                texture,
                view.ok_or_else(|| RecorderError::unsupported("Window fit has no input view."))?,
                size,
            ));
        }
        Ok(())
    }

    /// All GPU commands use the capture owner's context and completion query.
    /// The caller keeps the WGC frame alive until that query completes.
    pub(crate) fn draw(
        &self,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        region: D3D11_BOX,
        slot: usize,
    ) {
        let (texture, view, size) = self
            .input
            .as_ref()
            .expect("fit input preflighted before submission");
        let scale =
            (self.canvas.0 as f32 / size.0 as f32).min(self.canvas.1 as f32 / size.1 as f32);
        let width = size.0 as f32 * scale;
        let height = size.1 as f32 * scale;
        unsafe {
            context.CopySubresourceRegion(texture, 0, 0, 0, 0, source, 0, Some(&region));
            context.ClearRenderTargetView(&self.targets[slot], &[0., 0., 0., 1.]);
            context.OMSetRenderTargets(
                Some(&[Some(self.targets[slot].clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: (self.canvas.0 as f32 - width) * 0.5,
                TopLeftY: (self.canvas.1 as f32 - height) * 0.5,
                Width: width,
                Height: height,
                MinDepth: 0.,
                MaxDepth: 1.,
            }]));
            context.RSSetState(None::<&ID3D11RasterizerState>);
            context.OMSetBlendState(None::<&ID3D11BlendState>, None, u32::MAX);
            context.OMSetDepthStencilState(None::<&ID3D11DepthStencilState>, 0);
            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(view.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
        }
    }
}
