use gpui::{AssetSource, SharedString};
use std::borrow::Cow;
pub struct Assets;

/// The supplied opaque artwork is preserved; 64px covers the small UI mark at high DPI.
pub(crate) fn brand_mark(size: f32) -> gpui::Img {
    use gpui::{Styled, StyledImage};
    gpui::img("brand/freym_64px.png")
        .size(gpui::px(size))
        .object_fit(gpui::ObjectFit::Contain)
}
/// UI Lab's locally bundled typeface; no system font installation is needed.
pub fn install_fonts(cx: &gpui::App) -> anyhow::Result<()> {
    cx.text_system().add_fonts(vec![
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/ui-lab/FreymLabSans-400.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/ui-lab/FreymLabSans-500.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/ui-lab/FreymLabSans-600.ttf"
        )),
    ])
}
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "atmosphere.png" => Some(Cow::Borrowed(include_bytes!("../assets/atmosphere.png"))),
            "brand/freym_64px.png" => Some(Cow::Borrowed(include_bytes!(
                "../../../assets/brand/freym-logo-assets/freym_64px.png"
            ))),
            "brand/freym-mark.svg" => Some(Cow::Borrowed(include_bytes!(
                "../assets/brand/freym-mark.svg"
            ))),
            _ => None,
        })
    }
    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec![
            "atmosphere.png".into(),
            "brand/freym_64px.png".into(),
            "brand/freym-mark.svg".into(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_png_is_square_and_windows_icon_preserves_supplied_frames() {
        let png = Assets.load("brand/freym_64px.png").unwrap().unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 64);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 64);
        let official = include_bytes!("../../../assets/brand/freym-logo-assets/freym.ico");
        assert_eq!(
            official.as_slice(),
            include_bytes!("../../../assets/brand/icon.ico")
        );
        assert_eq!(&official[..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes(official[4..6].try_into().unwrap()) as usize;
        let sizes: Vec<_> = (0..count)
            .map(|i| {
                let n = official[6 + 16 * i];
                if n == 0 { 256 } else { u16::from(n) }
            })
            .collect();
        for required in [16, 24, 32, 48, 64, 128, 256] {
            assert!(sizes.contains(&required));
        }
    }
    #[test]
    fn supplied_titlebar_logo_renders_ink_without_an_opaque_background() {
        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(Assets));
        let bytes = Assets.load("brand/freym-mark.svg").unwrap().unwrap();
        let parsed = renderer.parse_svg(&bytes).unwrap();
        let image = renderer
            .render_parsed(
                &parsed,
                gpui::SvgSize::ExactSize(gpui::size(
                    gpui::DevicePixels(40),
                    gpui::DevicePixels(40),
                )),
            )
            .unwrap();
        let pixels = image.as_bytes(0).unwrap();
        let alpha: Vec<_> = pixels.chunks_exact(4).map(|p| p[3]).collect();
        for corner in [0, 39, 40 * 39, 40 * 40 - 1] {
            assert!(alpha[corner] < 8, "logo background must be transparent");
        }
        let ink = alpha.iter().filter(|a| **a > 127).count();
        assert!(
            ink > 100 && ink < 1200,
            "visible logo, not an empty or solid tile: {ink}"
        );
    }
}
