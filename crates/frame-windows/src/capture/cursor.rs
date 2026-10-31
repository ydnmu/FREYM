//! DXGI frame-bound pointer state. No OS cursor polling or desktop readback.

use std::ffi::c_void;
use std::mem::size_of;

use frame_contracts::RecorderError;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_MORE_DATA, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIOutputDuplication,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DI_NORMAL, DestroyIcon, DrawIconEx, HICON, ICONINFO,
};
use windows::core::BOOL;

use super::{internal_error, map_native_error};

// Includes driver-provided pitch padding and both monochrome mask planes.
const MAX_SHAPE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct FramePointer {
    position: Option<(i32, i32)>,
    icon: Option<OwnedIcon>,
    bytes: Vec<u8>,
    // Metadata only: enabled diagnostics never read desktop pixels or poll the OS.
    diagnostics: Option<PointerDiagnostics>,
}

#[derive(Default, serde::Serialize)]
struct PointerDiagnostics {
    update_sequence: u64,
    last_mouse_update_qpc: i64,
    shape_sequence: u64,
    shape: Option<ShapeDiagnostics>,
}

#[derive(serde::Serialize)]
struct ShapeDiagnostics {
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hotspot_x: i32,
    hotspot_y: i32,
}

impl FramePointer {
    fn observe_position(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            // Already physical, output-relative, top-left coordinates. Neither
            // desktop origin, DPI scaling nor hotspot subtraction belongs here.
            self.position = info.PointerPosition.Visible.as_bool().then_some((
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            ));
            if crate::cursor_preview_diagnostics::enabled() {
                let diagnostics = self.diagnostics.get_or_insert_with(Default::default);
                diagnostics.update_sequence = diagnostics.update_sequence.saturating_add(1);
                diagnostics.last_mouse_update_qpc = info.LastMouseUpdateTime;
            }
        }
    }

    pub(super) fn update(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<(), RecorderError> {
        self.observe_position(info);
        if info.PointerShapeBufferSize == 0 && (self.icon.is_some() || self.position.is_none()) {
            return Ok(());
        }
        let requested = info.PointerShapeBufferSize as usize;
        if requested > MAX_SHAPE_BYTES {
            return Err(internal_error(
                "DXGI pointer shape exceeds the 16 MiB budget.".into(),
            ));
        }
        self.bytes.resize(requested, 0);
        let mut required = 0;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the vector is initialized for exactly the advertised capacity;
        // a zero-size first query is allowed and reports the required capacity.
        let mut result = unsafe {
            duplication.GetFramePointerShape(
                self.bytes.len() as u32,
                self.bytes.as_mut_ptr().cast(),
                &mut required,
                &mut shape,
            )
        };
        if result
            .as_ref()
            .is_err_and(|error| error.code() == DXGI_ERROR_MORE_DATA)
        {
            if required == 0 || required as usize > MAX_SHAPE_BYTES {
                return Err(internal_error(
                    "DXGI pointer shape returned an invalid size.".into(),
                ));
            }
            self.bytes.resize(required as usize, 0);
            // SAFETY: same acquired frame; the required bounded buffer is live.
            result = unsafe {
                duplication.GetFramePointerShape(
                    self.bytes.len() as u32,
                    self.bytes.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape,
                )
            };
        }
        result.map_err(|error| map_native_error("GetFramePointerShape", error))?;
        if required as usize > self.bytes.len() {
            return Err(internal_error(
                "DXGI pointer shape exceeded its buffer.".into(),
            ));
        }
        // Replace only after construction succeeds; the old owned shape is
        // released once, including animated frames with an unchanged OS handle.
        self.icon = Some(OwnedIcon::from_shape(
            &shape,
            &self.bytes[..required as usize],
        )?);
        if crate::cursor_preview_diagnostics::enabled() {
            let diagnostics = self.diagnostics.get_or_insert_with(Default::default);
            diagnostics.shape_sequence = diagnostics.shape_sequence.saturating_add(1);
            diagnostics.shape = Some(ShapeDiagnostics {
                kind: shape.Type,
                width: shape.Width,
                height: shape.Height,
                pitch: shape.Pitch,
                hotspot_x: shape.HotSpot.x,
                hotspot_y: shape.HotSpot.y,
            });
        }
        Ok(())
    }

    /// Called only from the diagnostic record's lazy closure.
    pub(super) fn diagnostic_fields(&self) -> serde_json::Value {
        serde_json::json!({
            "position_output_physical": self.position,
            "separate_pointer_visible": self.visible(),
            "shape_available": self.icon.is_some(),
            "updates": self.diagnostics,
            "embedded_in_pixels": "unknown",
        })
    }

    pub(super) fn visible(&self) -> bool {
        self.position.is_some()
    }

    pub(super) fn draw(&self, dc: HDC) -> Result<(), RecorderError> {
        let Some((x, y)) = self.position else {
            return Ok(());
        };
        let icon = self.icon.as_ref().ok_or_else(|| {
            internal_error("DXGI reported a visible pointer without a captured shape.".into())
        })?;
        // SAFETY: the owned immutable shape and DC live throughout the draw.
        unsafe { DrawIconEx(dc, x, y, icon.0, 0, 0, 0, None, DI_NORMAL) }
            .map_err(|error| map_native_error("DrawIconEx(DXGI pointer)", error))
    }
}

struct OwnedBitmap(HBITMAP);

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        // SAFETY: this owned bitmap is not selected in a DC.
        let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
    }
}

struct OwnedIcon(HICON);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        // SAFETY: CreateIconIndirect gave this owner the handle.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

impl OwnedIcon {
    fn from_shape(
        shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        bytes: &[u8],
    ) -> Result<Self, RecorderError> {
        let kind = shape.Type as i32;
        let mono = kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0;
        let masked = kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
        if !mono && !masked && kind != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
            return Err(internal_error(format!(
                "Unknown DXGI pointer shape type {kind}."
            )));
        }
        let width = shape.Width as usize;
        let rows = shape.Height as usize;
        let height = if mono { rows / 2 } else { rows };
        let pitch = shape.Pitch as usize;
        let row_bytes = if mono {
            width.div_ceil(8)
        } else {
            width.saturating_mul(4)
        };
        let needed = pitch.saturating_mul(rows);
        if width == 0
            || height == 0
            || width > i32::MAX as usize
            || rows > i32::MAX as usize
            || (mono && !rows.is_multiple_of(2))
            || pitch < row_bytes
            || needed > bytes.len()
            || needed > MAX_SHAPE_BYTES
        {
            return Err(internal_error("Invalid DXGI pointer shape layout.".into()));
        }
        // CreateBitmap's 1bpp rows are WORD aligned, unlike DXGI's pitch.
        let mask_pitch = width.div_ceil(16) * 2;
        let mut mask = vec![0; mask_pitch * rows];
        let color = if mono {
            for row in 0..rows {
                mask[row * mask_pitch..row * mask_pitch + row_bytes]
                    .copy_from_slice(&bytes[row * pitch..row * pitch + row_bytes]);
            }
            None
        } else {
            let (color, pixels) = color_bitmap(width as i32, height as i32)?;
            // SAFETY: color_bitmap allocated width*height initialized 32bpp
            // pixels; only this thread accesses them before icon construction.
            let pixels = unsafe { std::slice::from_raw_parts_mut(pixels, width * height * 4) };
            for row in 0..height {
                for x in 0..width {
                    let source = &bytes[row * pitch + x * 4..row * pitch + x * 4 + 4];
                    let target = &mut pixels[(row * width + x) * 4..(row * width + x) * 4 + 4];
                    target.copy_from_slice(source);
                    if masked {
                        if source[3] != 0 && source[3] != 255 {
                            return Err(internal_error(
                                "Invalid masked-color DXGI pointer alpha.".into(),
                            ));
                        }
                        // Classic AND/XOR icon: alpha=0 replaces RGB;
                        // alpha=255 preserves the background then XORs RGB.
                        if source[3] == 255 {
                            mask[row * mask_pitch + x / 8] |= 0x80 >> (x % 8);
                        }
                        target[3] = 0;
                    } else {
                        // CreateIconIndirect prepares the alpha icon for GDI;
                        // premultiplying here as well would darken its edges.
                        if source[3] == 0 {
                            target[..3].fill(0);
                            mask[row * mask_pitch + x / 8] |= 0x80 >> (x % 8);
                        }
                    }
                }
            }
            Some(color)
        };
        // SAFETY: mask contains every WORD-aligned row, including both planes
        // for monochrome cursors. CreateBitmap copies the input before return.
        let mask = OwnedBitmap(unsafe {
            CreateBitmap(width as i32, rows as i32, 1, 1, Some(mask.as_ptr().cast()))
        });
        if mask.0.0.is_null() {
            return Err(map_native_error(
                "CreateBitmap(DXGI pointer mask)",
                windows::core::Error::from_win32(),
            ));
        }
        let info = ICONINFO {
            fIcon: BOOL(1),
            // DXGI position already identifies the image's top-left corner.
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask.0,
            hbmColor: color.as_ref().map_or(HBITMAP::default(), |bitmap| bitmap.0),
        };
        // SAFETY: both bitmaps remain live until CreateIconIndirect copies them.
        unsafe { CreateIconIndirect(&info) }
            .map(Self)
            .map_err(|error| map_native_error("CreateIconIndirect(DXGI pointer)", error))
    }
}

fn color_bitmap(width: i32, height: i32) -> Result<(OwnedBitmap, *mut u8), RecorderError> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: initialized top-down DIB descriptor and owned out-parameter.
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|error| map_native_error("CreateDIBSection(DXGI pointer)", error))?;
    let bitmap = OwnedBitmap(bitmap);
    if bits.is_null() {
        return Err(internal_error(
            "Pointer DIB returned no pixel storage.".into(),
        ));
    }
    Ok((bitmap, bits.cast()))
}

#[cfg(test)]
#[path = "cursor_gpu_tests.rs"]
mod gpu_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, GdiFlush, SelectObject};

    // Real GDI composition into owned memory, with no HWND, screen capture,
    // desktop cursor movement or GUI automation.
    struct Canvas {
        dc: HDC,
        bitmap: OwnedBitmap,
        previous: HGDIOBJ,
        bits: *mut u8,
        width: usize,
        height: usize,
    }

    impl Canvas {
        fn new(width: usize, height: usize) -> Self {
            let (bitmap, bits) = color_bitmap(width as i32, height as i32).unwrap();
            // SAFETY: private memory DC, and bitmap outlives its selection.
            let dc = unsafe { CreateCompatibleDC(None) };
            assert!(!dc.0.is_null());
            let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0.0)) };
            Self {
                dc,
                bitmap,
                previous,
                bits,
                width,
                height,
            }
        }

        fn clear(&mut self, rgb: [u8; 3]) {
            // SAFETY: this owned DIB has exactly width*height 32bpp pixels.
            let pixels =
                unsafe { std::slice::from_raw_parts_mut(self.bits, self.width * self.height * 4) };
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }

        fn rgb(&self, x: usize, y: usize) -> [u8; 3] {
            assert!(x < self.width && y < self.height);
            // SAFETY: flush GDI writes before CPU access; coordinates bounded above.
            assert!(unsafe { GdiFlush() }.as_bool());
            let bytes =
                unsafe { std::slice::from_raw_parts(self.bits.add((y * self.width + x) * 4), 3) };
            bytes.try_into().unwrap()
        }
    }

    impl Drop for Canvas {
        fn drop(&mut self) {
            // SAFETY: restore the old object before the owned bitmap is dropped.
            unsafe {
                SelectObject(self.dc, self.previous);
                let _ = DeleteDC(self.dc);
            }
            let _ = &self.bitmap;
        }
    }

    fn shape(kind: i32, width: u32, height: u32, pitch: u32) -> DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: kind as u32,
            Width: width,
            Height: height,
            Pitch: pitch,
            HotSpot: POINT { x: 1, y: 1 },
        }
    }

    fn observation(tick: i64, x: i32, y: i32, visible: bool) -> DXGI_OUTDUPL_FRAME_INFO {
        DXGI_OUTDUPL_FRAME_INFO {
            LastMouseUpdateTime: tick,
            PointerPosition: windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_POINTER_POSITION {
                Position: POINT { x, y },
                Visible: BOOL(i32::from(visible)),
            },
            ..Default::default()
        }
    }

    #[test]
    fn pointer_position_belongs_to_frame_and_is_retained_without_mouse_update() {
        let mut pointer = FramePointer::default();
        pointer.observe_position(&observation(100, -7, 42, true));
        assert_eq!(pointer.position, Some((-7, 42)));
        pointer.observe_position(&observation(0, 999, 999, false));
        assert_eq!(pointer.position, Some((-7, 42)));
        // A pressure-dropped acquisition's update must still be retained.
        pointer.observe_position(&observation(110, 24, 50, true));
        pointer.observe_position(&observation(0, 0, 0, false));
        assert_eq!(pointer.position, Some((24, 50)));
        pointer.observe_position(&observation(120, 24, 50, false));
        assert!(!pointer.visible());
    }

    #[test]
    fn controlled_pointer_trajectory_has_one_image_no_trails_and_no_hotspot_offset() {
        let icon = OwnedIcon::from_shape(
            &shape(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0, 2, 2, 8),
            &[255; 16],
        )
        .unwrap();
        let mut pointer = FramePointer {
            icon: Some(icon),
            ..Default::default()
        };
        let mut canvas = Canvas::new(32, 16);
        for frame in 1..=96 {
            let x = (frame * 7 % 36) - 2;
            let y = (frame * 3 % 20) - 2;
            canvas.clear([17, 31, 63]); // Fresh desktop copy for every output slot.
            pointer.observe_position(&observation(frame.into(), x, y, true));
            pointer.draw(canvas.dc).unwrap();
            for py in 0..16 {
                for px in 0..32 {
                    let inside =
                        (x..x + 2).contains(&(px as i32)) && (y..y + 2).contains(&(py as i32));
                    assert_eq!(
                        canvas.rgb(px, py),
                        if inside { [255; 3] } else { [17, 31, 63] },
                        "frame {frame}, pixel {px},{py}"
                    );
                }
            }
        }
        canvas.clear([17, 31, 63]);
        pointer.observe_position(&observation(100, 4, 4, false));
        pointer.draw(canvas.dc).unwrap();
        assert_eq!(canvas.rgb(4, 4), [17, 31, 63]);
    }

    #[test]
    fn color_pointer_respects_alpha_pitch_and_top_down_rows() {
        let info = shape(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0, 2, 2, 12);
        let bytes = [
            255, 0, 0, 128, 0, 255, 0, 255, 99, 99, 99, 99, 0, 0, 255, 255, 255, 255, 255, 0, 99,
            99, 99, 99,
        ];
        let pointer = FramePointer {
            position: Some((0, 0)),
            icon: Some(OwnedIcon::from_shape(&info, &bytes).unwrap()),
            ..Default::default()
        };
        let mut canvas = Canvas::new(2, 2);
        canvas.clear([0; 3]);
        pointer.draw(canvas.dc).unwrap();
        assert_eq!(canvas.rgb(0, 0), [128, 0, 0]);
        assert_eq!(canvas.rgb(1, 0), [0, 255, 0]);
        assert_eq!(canvas.rgb(0, 1), [0, 0, 255]);
        assert_eq!(canvas.rgb(1, 1), [0, 0, 0]);
    }

    #[test]
    fn masked_color_pointer_replaces_or_xors_instead_of_alpha_blending() {
        let info = shape(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0, 2, 1, 8);
        let pointer = FramePointer {
            position: Some((0, 0)),
            icon: Some(
                OwnedIcon::from_shape(&info, &[7, 19, 37, 0, 0xff, 0x0f, 0xf0, 255]).unwrap(),
            ),
            ..Default::default()
        };
        let mut canvas = Canvas::new(2, 1);
        canvas.clear([0x12, 0x34, 0x56]);
        pointer.draw(canvas.dc).unwrap();
        assert_eq!(canvas.rgb(0, 0), [7, 19, 37]);
        assert_eq!(canvas.rgb(1, 0), [0xed, 0x3b, 0xa6]);
    }

    #[test]
    fn monochrome_pointer_preserves_both_mask_planes_and_row_order() {
        let info = shape(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0, 4, 4, 1);
        // AND rows, then XOR rows: black/white/transparent/invert, reversed below.
        let pointer = FramePointer {
            position: Some((0, 0)),
            icon: Some(OwnedIcon::from_shape(&info, &[0x30, 0xc0, 0x50, 0xa0]).unwrap()),
            ..Default::default()
        };
        let mut canvas = Canvas::new(4, 2);
        canvas.clear([0x12, 0x34, 0x56]);
        pointer.draw(canvas.dc).unwrap();
        let expected = [[0; 3], [255; 3], [0x12, 0x34, 0x56], [0xed, 0xcb, 0xa9]];
        for (x, rgb) in expected.into_iter().enumerate() {
            assert_eq!(canvas.rgb(x, 0), rgb);
            assert_eq!(canvas.rgb(3 - x, 1), rgb);
        }
    }

    #[test]
    fn changed_shape_at_same_position_replaces_the_previous_image() {
        let info = shape(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0, 1, 1, 4);
        let mut pointer = FramePointer {
            position: Some((0, 0)),
            ..Default::default()
        };
        let mut canvas = Canvas::new(1, 1);
        for rgb in [[255, 0, 0], [0, 255, 0], [0, 0, 255]] {
            pointer.icon =
                Some(OwnedIcon::from_shape(&info, &[rgb[0], rgb[1], rgb[2], 255]).unwrap());
            canvas.clear([0; 3]);
            pointer.draw(canvas.dc).unwrap();
            assert_eq!(canvas.rgb(0, 0), rgb);
        }
    }

    #[test]
    fn dropped_pointer_mode_transition_is_retained_before_the_next_draw() {
        let info = shape(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0, 1, 1, 4);
        let mut pointer = FramePointer {
            icon: Some(OwnedIcon::from_shape(&info, &[255; 4]).unwrap()),
            ..Default::default()
        };
        let mut canvas = Canvas::new(8, 1);
        pointer.observe_position(&observation(100, 0, 0, true));
        canvas.clear([17, 31, 63]);
        pointer.draw(canvas.dc).unwrap();
        assert_eq!(canvas.rgb(0, 0), [255; 3]);

        // This real metadata update is retained even when its image is dropped.
        pointer.observe_position(&observation(110, 3, 0, false));
        // A desktop-only update supplies no valid pointer observation. Its
        // arbitrary Visible/coords must not replace the retained state.
        pointer.observe_position(&observation(0, 99, 99, true));
        canvas.clear([17, 31, 63]);
        pointer.draw(canvas.dc).unwrap();
        for x in 0..8 {
            assert_eq!(canvas.rgb(x, 0), [17, 31, 63]);
        }

        pointer.observe_position(&observation(120, 6, 0, true));
        pointer.observe_position(&observation(0, 0, 0, false));
        canvas.clear([17, 31, 63]);
        pointer.draw(canvas.dc).unwrap();
        for x in 0..8 {
            assert_eq!(
                canvas.rgb(x, 0),
                if x == 6 { [255; 3] } else { [17, 31, 63] }
            );
        }
    }

    #[test]
    fn malformed_shapes_fail_without_native_pointer_access() {
        for info in [
            shape(99, 1, 1, 4),
            shape(2, 0, 1, 4),
            shape(2, 2, 2, 4),
            shape(1, 1, 3, 1),
            shape(2, u32::MAX, u32::MAX, u32::MAX),
        ] {
            assert!(OwnedIcon::from_shape(&info, &[0; 16]).is_err());
        }
        assert!(OwnedIcon::from_shape(&shape(2, 2, 2, 8), &[0; 15]).is_err());
        assert!(OwnedIcon::from_shape(&shape(4, 1, 1, 4), &[0, 0, 0, 42]).is_err());
    }
}
