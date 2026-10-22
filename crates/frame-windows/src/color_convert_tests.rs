//! Offscreen hardware pixel tests. No HWND, desktop capture or pointer input.
use super::*;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

struct Fixture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}
impl Fixture {
    fn new() -> Self {
        let mut device = None;
        let mut context = None;
        // SAFETY: no output/window; owned initialized device out-parameters.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .unwrap();
        Self {
            device: device.unwrap(),
            context: context.unwrap(),
        }
    }
    fn texture(&self, format: DXGI_FORMAT, width: u32, bindings: u32) -> ID3D11Texture2D {
        let mut texture = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bindings,
            ..Default::default()
        };
        // SAFETY: initialized bounded descriptor and owned out-parameter.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
        texture.unwrap()
    }
    fn read(&self, source: &ID3D11Texture2D, bytes: usize) -> Vec<u8> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.unwrap();
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: fixture-only blocking map after copy to a CPU-readable texture.
        unsafe {
            self.context.CopyResource(&staging, source);
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .unwrap();
        assert!(bytes <= mapped.RowPitch as usize);
        let output =
            unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), bytes) }.to_vec();
        unsafe { self.context.Unmap(&staging, 0) };
        output
    }
}

#[test]
fn format_color_fp16_first_frame_has_real_sdr_pixels_without_a_cache() {
    let f = Fixture::new();
    let source = f.texture(
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        7,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
    );
    let output = f.texture(
        DXGI_FORMAT_B8G8R8A8_UNORM,
        7,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    );
    // Linear RGBA: black, gray .25, gray .5, white, red, green, blue.
    let pixels: [u16; 28] = [
        0, 0, 0, 0x3c00, 0x3400, 0x3400, 0x3400, 0x3c00, 0x3800, 0x3800, 0x3800, 0x3c00, 0x3c00,
        0x3c00, 0x3c00, 0x3c00, 0x3c00, 0, 0, 0x3c00, 0, 0x3c00, 0, 0x3c00, 0, 0, 0x3c00, 0x3c00,
    ];
    unsafe {
        f.context
            .UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), 56, 0)
    };
    let pass = GpuColorPass::new(
        &f.device,
        &source,
        std::slice::from_ref(&output),
        ColorDirection::LinearToSdr,
    )
    .unwrap();
    pass.draw(&f.context, &output).unwrap();
    let expected = [
        [0, 0, 0, 255],
        [137, 137, 137, 255],
        [188, 188, 188, 255],
        [255; 4],
        [0, 0, 255, 255],
        [0, 255, 0, 255],
        [255, 0, 0, 255],
    ];
    for (actual, expected) in f.read(&output, 28).chunks_exact(4).zip(expected) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                (*a as i16 - e as i16).abs() <= 1,
                "{actual:?} != {expected:?}"
            );
        }
    }
}

#[test]
fn format_color_preview_linear_round_trip_preserves_all_sdr_gray_values_and_primaries() {
    let f = Fixture::new();
    let width = 259;
    let source = f.texture(
        DXGI_FORMAT_B8G8R8A8_UNORM,
        width,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
    );
    let linear = f.texture(
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        width,
        (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
    );
    let output = f.texture(
        DXGI_FORMAT_B8G8R8A8_UNORM,
        width,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    );
    let mut pixels: Vec<u8> = (0..=255u8).flat_map(|c| [c, c, c, 255]).collect();
    pixels.extend_from_slice(&[0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255]);
    unsafe {
        f.context
            .UpdateSubresource(&source, 0, None, pixels.as_ptr().cast(), width * 4, 0)
    };
    GpuColorPass::new(
        &f.device,
        &source,
        std::slice::from_ref(&linear),
        ColorDirection::SdrToLinear,
    )
    .unwrap()
    .draw(&f.context, &linear)
    .unwrap();
    let fp16 = f.read(&linear, width as usize * 8);
    // Gray 128 must be linear ~0.216, not encoded ~0.502 stored as FP16.
    let bits = u16::from_le_bytes(fp16[128 * 8..128 * 8 + 2].try_into().unwrap());
    let linear_gray =
        (1.0 + (bits & 1023) as f64 / 1024.0) * 2f64.powi(((bits >> 10) & 31) as i32 - 15);
    assert!((linear_gray - 0.21586).abs() < 0.001);
    GpuColorPass::new(
        &f.device,
        &linear,
        std::slice::from_ref(&output),
        ColorDirection::LinearToSdr,
    )
    .unwrap()
    .draw(&f.context, &output)
    .unwrap();
    for (a, e) in f.read(&output, pixels.len()).iter().zip(&pixels) {
        assert!((*a as i16 - *e as i16).abs() <= 1);
    }
}

#[test]
fn format_color_rejects_incompatible_layout_before_drawing() {
    let f = Fixture::new();
    let source = f.texture(
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        2,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
    );
    let wrong_size = f.texture(
        DXGI_FORMAT_B8G8R8A8_UNORM,
        1,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    );
    assert!(
        GpuColorPass::new(
            &f.device,
            &source,
            &[wrong_size],
            ColorDirection::LinearToSdr
        )
        .is_err()
    );
    let wrong_format = f.texture(
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        2,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    );
    assert!(
        GpuColorPass::new(
            &f.device,
            &source,
            &[wrong_format],
            ColorDirection::LinearToSdr
        )
        .is_err()
    );
}
