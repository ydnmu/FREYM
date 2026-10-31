#![allow(unsafe_code)] // Same-device D3D11 color pass; no pixel mapping or GPU waits.
//! SDR sRGB <-> linear scRGB conversion for native capture and presentation.
//! HDR recording retains its separate, explicit PQ/BT.2020 pipeline.

use frame_contracts::RecorderError;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::core::Interface;

const SHADER: &[u8] = br#"
Texture2D<float4> sourceTexture : register(t0);
struct VertexOut { float4 position : SV_POSITION; };
VertexOut vs(uint vertex : SV_VertexID) {
    VertexOut output;
    float2 positions[3] = { float2(-1,-1), float2(-1,3), float2(3,-1) };
    output.position = float4(positions[vertex], 0, 1);
    return output;
}
float encodeSRGB(float value) {
    value = saturate(value);
    return value <= 0.0031308 ? value * 12.92 : 1.055 * pow(value, 1.0/2.4) - 0.055;
}
float decodeSRGB(float encoded) {
    encoded = saturate(encoded);
    return encoded <= 0.04045 ? encoded / 12.92 : pow((encoded + 0.055) / 1.055, 2.4);
}
float4 toSDR(VertexOut input) : SV_Target {
    float3 c = sourceTexture.Load(int3(int2(input.position.xy), 0)).rgb;
    return float4(encodeSRGB(c.r), encodeSRGB(c.g), encodeSRGB(c.b), 1);
}
float4 toLinear(VertexOut input) : SV_Target {
    float3 c = sourceTexture.Load(int3(int2(input.position.xy), 0)).rgb;
    return float4(decodeSRGB(c.r), decodeSRGB(c.g), decodeSRGB(c.b), 1);
}
"#;

#[derive(Clone, Copy)]
pub(crate) enum ColorDirection {
    LinearToSdr,
    /// Consumer parity: only the offscreen tests exercise the reverse direction.
    #[cfg_attr(not(test), allow(dead_code))]
    SdrToLinear,
}
impl ColorDirection {
    fn formats(self) -> (DXGI_FORMAT, DXGI_FORMAT) {
        match self {
            Self::LinearToSdr => (DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM),
            Self::SdrToLinear => (DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT),
        }
    }
}

fn failure(message: impl Into<String>) -> RecorderError {
    RecorderError::unsupported(&format!("GPU SDR color conversion: {}", message.into()))
}

pub(crate) struct GpuColorPass {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    source: ID3D11ShaderResourceView,
    targets: Vec<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    width: u32,
    height: u32,
}
impl GpuColorPass {
    pub(crate) fn new(
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
        targets: &[ID3D11Texture2D],
        direction: ColorDirection,
    ) -> Result<Self, RecorderError> {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: live resource and initialized metadata output.
        unsafe { source.GetDesc(&mut source_desc) };
        let (input_format, output_format) = direction.formats();
        if source_desc.Format != input_format
            || source_desc.Width == 0
            || source_desc.Height == 0
            || source_desc.ArraySize != 1
            || source_desc.MipLevels != 1
            || source_desc.SampleDesc.Count != 1
            || source_desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0
            || targets.is_empty()
            || targets.len() > 4
        {
            return Err(failure("invalid single-sample input or bounded target set"));
        }
        let mut views = Vec::with_capacity(targets.len());
        for target in targets {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: live texture and owned out-parameters; views retain resources.
            unsafe { target.GetDesc(&mut desc) };
            if desc.Format != output_format
                || desc.Width != source_desc.Width
                || desc.Height != source_desc.Height
                || desc.ArraySize != 1
                || desc.MipLevels != 1
                || desc.SampleDesc.Count != 1
                || desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 == 0
            {
                return Err(failure(
                    "target format/geometry differs from its color pass",
                ));
            }
            let mut view = None;
            unsafe { device.CreateRenderTargetView(target, None, Some(&mut view)) }
                .map_err(|e| failure(e.to_string()))?;
            views.push((
                target.clone(),
                view.ok_or_else(|| failure("target view missing"))?,
            ));
        }
        let vertex_code = crate::compositor::compile(SHADER, b"vs\0", b"vs_4_0\0")?;
        let pixel_entry: &'static [u8] = match direction {
            ColorDirection::LinearToSdr => b"toSDR\0",
            ColorDirection::SdrToLinear => b"toLinear\0",
        };
        let pixel_code = crate::compositor::compile(SHADER, pixel_entry, b"ps_4_0\0")?;
        let mut vertex = None;
        let mut pixel = None;
        let mut source_view = None;
        // SAFETY: valid compiled bytecode, live source, initialized outputs.
        unsafe {
            device
                .CreateVertexShader(&vertex_code, None::<&ID3D11ClassLinkage>, Some(&mut vertex))
                .map_err(|e| failure(e.to_string()))?;
            device
                .CreatePixelShader(&pixel_code, None::<&ID3D11ClassLinkage>, Some(&mut pixel))
                .map_err(|e| failure(e.to_string()))?;
            device
                .CreateShaderResourceView(source, None, Some(&mut source_view))
                .map_err(|e| failure(e.to_string()))?;
        }
        Ok(Self {
            vertex: vertex.ok_or_else(|| failure("vertex shader missing"))?,
            pixel: pixel.ok_or_else(|| failure("pixel shader missing"))?,
            source: source_view.ok_or_else(|| failure("input view missing"))?,
            targets: views,
            width: source_desc.Width,
            height: source_desc.Height,
        })
    }

    pub(crate) fn draw(
        &self,
        context: &ID3D11DeviceContext,
        target: &ID3D11Texture2D,
    ) -> Result<(), RecorderError> {
        let view = self
            .targets
            .iter()
            .find(|(texture, _)| texture.as_raw() == target.as_raw())
            .map(|(_, view)| view)
            .ok_or_else(|| failure("unregistered target texture"))?;
        let multi: ID3D11Multithread = context.cast().map_err(|e| failure(e.to_string()))?;
        // Protect the short shader-state batch against encoder use of the same
        // immediate context. No Flush, Map, query polling or GPU wait is added.
        // SAFETY: live context; every Enter is balanced after void command calls.
        unsafe {
            multi.Enter();
            context
                .OMSetRenderTargets(Some(&[Some(view.clone())]), None::<&ID3D11DepthStencilView>);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: self.width as f32,
                Height: self.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            context.RSSetState(None::<&ID3D11RasterizerState>);
            context.OMSetBlendState(None::<&ID3D11BlendState>, None, u32::MAX);
            context.OMSetDepthStencilState(None::<&ID3D11DepthStencilState>, 0);
            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.GSSetShader(None::<&ID3D11GeometryShader>, None);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(self.source.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
            multi.Leave();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "color_convert_tests.rs"]
mod tests;
