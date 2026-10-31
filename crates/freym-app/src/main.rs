#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use freym_ui::ui::{components::Titlebar, theme::spacing::Layout};
use gpui::{App, AppContext, Bounds, px, size};

fn main() {
    #[cfg(target_os = "windows")]
    let trace_guard = match frame_windows::cursor_preview_diagnostics::start_from_env() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("Cursor/preview trace unavailable: {error}");
            None
        }
    };
    // The existing capture/encoder helpers must run before initializing GPUI.
    #[cfg(target_os = "windows")]
    match std::env::args().nth(1).as_deref() {
        Some("--frame-wgc-helper") => {
            let result = frame_windows::run_wgc_helper();
            drop(trace_guard);
            std::process::exit(result);
        }
        Some("--frame-mf-helper") => std::process::exit(frame_windows::run_mf_helper()),
        _ => {}
    }
    let _update_guard = match freym_update::install::application_guard() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("Could not open Freym: {error:#}");
            freym_update::show_launch_error(&error);
            return;
        }
    };
    #[cfg(target_os = "windows")]
    let _trace_guard = trace_guard;
    gpui_platform::application()
        .with_assets(freym_ui::Assets)
        .run(|cx: &mut App| {
            freym_ui::install_fonts(cx).expect("FREYM control fonts could not be loaded");
            let bounds = Bounds::centered(
                None,
                size(px(Layout::WINDOW_WIDTH), px(Layout::WINDOW_HEIGHT)),
                cx,
            );
            let mut options = Titlebar::window_options("FREYM", bounds, "dev.frame.recorder");
            // Embedded child HWND stays below GPUI's composition visual.
            // Only its viewport is left transparent by RecorderScreen.
            options.window_background = gpui::WindowBackgroundAppearance::Transparent;
            cx.open_window(options, |window, cx| {
                cx.new(|cx| freym_ui::Home::new(window, cx))
            })
            .expect("FREYM native window creation failed");
            cx.background_executor()
                .spawn(async {
                    let _ = freym_update::install::mark_healthy();
                })
                .detach();
            freym_ui::bind_keys(cx);
            cx.activate(true);
        });
}
