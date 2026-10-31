//! Pixel regressions for the actual D3D11 copy/GDI composition boundary.
//! All textures and pointer shapes are synthetic. No HWND or desktop capture.

use super::*;
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const WIDTH: u32 = 32;
const HEIGHT: u32 = 16;
const BACKGROUND: [u8; 4] = [17, 31, 63, 255];

struct Fixture {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    raw: ID3D11Texture2D,
    slots: Vec<ID3D11Texture2D>,
    staging: ID3D11Texture2D,
}

impl Fixture {
    fn fp16_resource(&self) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: owned initialized descriptor, no capture source or window.
        unsafe {
            self._device
                .CreateTexture2D(&desc, None, Some(&mut texture))
        }
        .unwrap();
        let texture = texture.unwrap();
        let pixels = [0x3400u16, 0x3800, 0x3a00, 0x3c00].repeat((WIDTH * HEIGHT) as usize);
        unsafe {
            self.context
                .UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), WIDTH * 8, 0);
        }
        texture
    }

    fn new() -> Self {
        let mut device = None;
        let mut context = None;
        // SAFETY: no window/output is involved; owned synthetic device outputs.
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
        .expect("offscreen D3D11 device");
        let device = device.unwrap();
        let context = context.unwrap();
        let desc = D3D11_TEXTURE2D_DESC {
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
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
            ..Default::default()
        };
        let create = |desc: &D3D11_TEXTURE2D_DESC| {
            let mut texture = None;
            // SAFETY: initialized descriptor; resource retained by this fixture.
            unsafe { device.CreateTexture2D(desc, None, Some(&mut texture)) }.unwrap();
            texture.unwrap()
        };
        let raw = create(&desc);
        let slots = (0..4).map(|_| create(&desc)).collect();
        let staging = create(&D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            MiscFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..desc
        });
        let fixture = Self {
            _device: device,
            context,
            raw,
            slots,
            staging,
        };
        fixture.reset_raw();
        fixture
    }

    fn reset_raw(&self) {
        let pixels = BACKGROUND.repeat((WIDTH * HEIGHT) as usize);
        // SAFETY: initialized full BGRA image, exact row pitch, live resources.
        unsafe {
            self.context.UpdateSubresource(
                &self.raw,
                0,
                None,
                pixels.as_ptr().cast(),
                WIDTH * 4,
                0,
            );
        }
    }

    fn compose(&self, slot: usize, pointer: &FramePointer) {
        // This is the production sequence, including the same GDI surface call.
        // SAFETY: independent textures on this owned immediate context.
        let (background, reused) =
            super::super::frame_background(&self.raw, &self.slots[slot], None).unwrap();
        assert!(!reused);
        unsafe { self.context.CopyResource(&self.slots[slot], background) };
        super::super::draw_cursor_on_surface(&self.slots[slot], pointer, None).unwrap();
        unsafe { self.context.Flush() };
    }

    fn pixels(&self, texture: &ID3D11Texture2D) -> Vec<[u8; 4]> {
        // Readback exists only in this test, after the production composition.
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: owned CPU-readable staging; Map waits for its preceding copy.
        unsafe {
            self.context.CopyResource(&self.staging, texture);
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .unwrap();
        let mut pixels = Vec::with_capacity((WIDTH * HEIGHT) as usize);
        for y in 0..HEIGHT {
            // SAFETY: initialized mapped rows and exact valid width, ignoring padding.
            let row = unsafe {
                std::slice::from_raw_parts(
                    mapped
                        .pData
                        .cast::<u8>()
                        .add((y * mapped.RowPitch) as usize),
                    (WIDTH * 4) as usize,
                )
            };
            pixels.extend(
                row.chunks_exact(4)
                    .map(|pixel| <[u8; 4]>::try_from(pixel).unwrap()),
            );
        }
        // SAFETY: balances the single successful Map after all borrowed reads.
        unsafe { self.context.Unmap(&self.staging, 0) };
        pixels
    }
}

#[test]
fn pointer_only_fp16_resource_does_not_accumulate_cursor_pixels() {
    let fixture = Fixture::new();
    let pointer_only = fixture.fp16_resource();
    for slot in 0..fixture.slots.len() {
        fixture.compose(slot, &pointer(2, 2));
    }
    // Same format=10, LastPresentTime=0 resource seen on this machine. The
    // production selector uses the clean BGRA8 desktop before drawing once.
    for frame in 0..20 {
        let slot = frame % fixture.slots.len();
        let (background, reused) =
            super::super::frame_background(&pointer_only, &fixture.slots[slot], Some(&fixture.raw))
                .unwrap();
        assert!(reused);
        unsafe {
            fixture
                .context
                .CopyResource(&fixture.slots[slot], background)
        };
        let (x, y) = if frame < 16 {
            ((frame * 7 % 28) as i32, (frame * 3 % 12) as i32)
        } else {
            (21, 9)
        };
        super::super::draw_cursor_on_surface(&fixture.slots[slot], &pointer(x, y), None).unwrap();
        assert_one_cursor(&fixture.pixels(&fixture.slots[slot]), x, y);
    }
    assert_eq!(
        fixture.pixels(&fixture.raw),
        vec![BACKGROUND; (WIDTH * HEIGHT) as usize]
    );
}

#[test]
fn incompatible_desktop_without_current_background_is_rejected_before_drawing() {
    let fixture = Fixture::new();
    fixture.compose(0, &pointer(2, 2));
    let before = fixture.pixels(&fixture.slots[0]);
    let incompatible = fixture.fp16_resource();
    let error = super::super::frame_background(&incompatible, &fixture.slots[0], None)
        .expect_err("an incompatible desktop must never be blindly copied");
    assert_eq!(error.code, frame_contracts::ErrorCode::SourceLost);
    assert_eq!(fixture.pixels(&fixture.slots[0]), before);
    // A composited FP16 image is also not an eligible BGRA background.
    assert!(
        super::super::frame_background(&incompatible, &fixture.slots[0], Some(&incompatible))
            .is_err()
    );
}

fn pointer(x: i32, y: i32) -> FramePointer {
    let shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        Type: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
        Width: 2,
        Height: 2,
        Pitch: 8,
        HotSpot: POINT { x: 1, y: 1 },
    };
    FramePointer {
        position: Some((x, y)),
        icon: Some(OwnedIcon::from_shape(&shape, &[255; 16]).unwrap()),
        ..Default::default()
    }
}

#[test]
fn format_color_cold_fp16_and_changed_background_do_not_retain_old_cursor_pixels() {
    use crate::color_convert::{ColorDirection, GpuColorPass};
    let fixture = Fixture::new();
    let native = fixture.fp16_resource(); // Same bind-less layout as a driver resource.
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { native.GetDesc(&mut desc) };
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
    let mut input = None;
    unsafe {
        fixture
            ._device
            .CreateTexture2D(&desc, None, Some(&mut input))
    }
    .unwrap();
    let input = input.unwrap();
    let pass = GpuColorPass::new(
        &fixture._device,
        &input,
        &fixture.slots,
        ColorDirection::LinearToSdr,
    )
    .unwrap();
    for frame in 0..12 {
        let slot = frame % fixture.slots.len();
        // Changed FP16 background represents a first frame or a desktop update
        // that cannot legally reuse a previous capture cache.
        let level = if frame % 2 == 0 { 0x3400u16 } else { 0x3800u16 };
        let rgb = if frame % 2 == 0 { 137u8 } else { 188u8 };
        let pixels = [level, level, level, 0x3c00].repeat((WIDTH * HEIGHT) as usize);
        unsafe {
            fixture.context.UpdateSubresource(
                &native,
                0,
                None,
                pixels.as_ptr().cast(),
                WIDTH * 8,
                0,
            );
            fixture.context.CopyResource(&input, &native);
        }
        pass.draw(&fixture.context, &fixture.slots[slot]).unwrap();
        let x = (frame * 5 % 28) as i32;
        super::super::draw_cursor_on_surface(&fixture.slots[slot], &pointer(x, 6), None).unwrap();
        for (index, pixel) in fixture.pixels(&fixture.slots[slot]).iter().enumerate() {
            let px = index as i32 % WIDTH as i32;
            let py = index as i32 / WIDTH as i32;
            let expected = if (x..x + 2).contains(&px) && (6..8).contains(&py) {
                255
            } else {
                rgb
            };
            for &channel in &pixel[..3] {
                assert!((channel as i16 - expected as i16).abs() <= 1);
            }
        }
    }
}

fn assert_one_cursor(pixels: &[[u8; 4]], x: i32, y: i32) {
    for py in 0..HEIGHT {
        for px in 0..WIDTH {
            let expected = if (x..x + 2).contains(&(px as i32)) && (y..y + 2).contains(&(py as i32))
            {
                [255; 3]
            } else {
                BACKGROUND[..3].try_into().unwrap()
            };
            assert_eq!(
                pixels[(py * WIDTH + px) as usize][..3],
                expected,
                "pixel {px},{py}"
            );
        }
    }
}

#[test]
fn reused_gpu_slots_remove_old_cursor_without_mutating_raw_pixels() {
    let fixture = Fixture::new();
    for frame in 0..24 {
        let x = (frame * 7 % 34) as i32 - 2;
        let y = (frame * 3 % 18) as i32 - 2;
        let slot = frame % fixture.slots.len();
        fixture.compose(slot, &pointer(x, y));
        assert_one_cursor(&fixture.pixels(&fixture.slots[slot]), x, y);
        assert_eq!(
            fixture.pixels(&fixture.raw),
            vec![BACKGROUND; (WIDTH * HEIGHT) as usize]
        );
    }
    // Reusing each of the four slots after motion stops must retain only the last position.
    for slot in 0..fixture.slots.len() {
        fixture.compose(slot, &pointer(13, 7));
        assert_one_cursor(&fixture.pixels(&fixture.slots[slot]), 13, 7);
    }
}

#[test]
fn gpu_slot_copy_keeps_embedded_cursor_without_a_second_draw() {
    let fixture = Fixture::new();
    // Model raw pixels with an already embedded pointer, and no separate pointer.
    super::super::draw_cursor_on_surface(&fixture.raw, &pointer(4, 6), None).unwrap();
    fixture.compose(0, &FramePointer::default());
    assert_one_cursor(&fixture.pixels(&fixture.slots[0]), 4, 6);
    // A later separate cursor uses a new raw background, removing embedded pixels.
    fixture.reset_raw();
    fixture.compose(0, &pointer(21, 9));
    assert_one_cursor(&fixture.pixels(&fixture.slots[0]), 21, 9);
}
