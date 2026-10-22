use gpui::{AssetSource, SharedString};
use std::borrow::Cow;
pub struct Assets;
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
            _ => None,
        })
    }
    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec!["atmosphere.png".into()])
    }
}
