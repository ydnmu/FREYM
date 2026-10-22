#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
use freym_ui::{PreviewWorkbench, ui::theme::spacing::Layout};
use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

fn main() {
    gpui_platform::application()
        .with_assets(freym_ui::Assets)
        .run(|cx: &mut App| {
            freym_ui::install_fonts(cx).expect("UI Lab reference font registration failed");
            let bounds = Bounds::centered(None, size(px(Layout::WINDOW_WIDTH), px(940.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("FREYM · GPUI UI Lab".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(Layout::MIN_WINDOW_WIDTH), px(740.))),
                    app_id: Some("dev.frame.recorder.ui-preview".into()),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| PreviewWorkbench::new(window, cx)),
            )
            .expect("FREYM UI preview window creation failed");
            freym_ui::bind_keys(cx);
            cx.activate(true);
        });
}
