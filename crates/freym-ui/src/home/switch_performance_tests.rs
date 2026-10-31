//! Opt-in, bounded CPU probe on GPUI's TestPlatform. Never opens a native window.
//! TestPlatform does not present to a GPU/compositor; these are not display FPS.
use super::*;
use gpui::{TestAppContext, point, size};
use std::{cell::Cell, time::Instant};

thread_local! { static RENDER_NS: Cell<u64> = const { Cell::new(0) }; }
pub(super) struct RenderTiming(Instant);
impl RenderTiming {
    pub fn start() -> Self {
        Self(Instant::now())
    }
}
impl Drop for RenderTiming {
    fn drop(&mut self) {
        RENDER_NS.with(|n| n.set(n.get() + self.0.elapsed().as_nanos() as u64));
    }
}
fn report(label: &str, values: &mut [f64]) {
    values.sort_by(f64::total_cmp);
    println!(
        "{label}: n={} p50={:.3} p95={:.3} max={:.3} ms",
        values.len(),
        values[values.len() / 2],
        values[values.len() * 95 / 100],
        values[values.len() - 1]
    );
}

#[gpui::test]
#[ignore = "bounded manual CPU timing probe; run explicitly with --ignored --nocapture"]
fn switch_settings_cpu_probe(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.simulate_resize(size(px(1280.), px(720.)));
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            let (request, capabilities) = crate::video::adapter::tests::fixture();
            home.model.request = Some(request);
            home.video.capabilities = Some(capabilities);
            home.model.sheet = Some(Sheet::Settings);
            home.settings_category = 5;
            home.model.preferences.reduced_motion = false;
            cx.set_reduce_motion(false);
            cx.notify();
        })
    });
    cx.run_until_parked();
    for scenario in ["switch", "scroll"] {
        let mut input = Vec::new();
        let mut draw = Vec::new();
        let mut reconstruction = Vec::new();
        let mut remainder = Vec::new();
        let mut callback_counts = Vec::new();
        if scenario == "scroll" {
            cx.update(|_, cx| {
                view.update(cx, |home, cx| {
                    home.settings_category = 1;
                    home.video.mode = crate::video::VideoMode::Advanced;
                    home.video.expanded.fill(true);
                    cx.notify();
                })
            });
            cx.run_until_parked();
        }
        let renders_before = cx.update(|_, cx| view.read(cx).render_count);
        for frame in 0..72 {
            // The real Instant is used by pinned GPUI's spring implementation.
            // Sleep only for animation sampling, never for the CPU draw metric.
            if scenario == "switch" {
                std::thread::sleep(Duration::from_millis(16));
            }
            cx.update(|window, cx| {
                let start = Instant::now();
                if scenario == "switch" && frame % 24 == 0 {
                    view.update(cx, |home, cx| {
                        home.dispatch(Action::FilledButtons, window, cx)
                    });
                } else if scenario == "scroll" {
                    view.update(cx, |home, _| {
                        home.settings_scroll
                            .set_offset(point(px(0.), px(-((frame % 30) as f32) * 12.)))
                    });
                    window.refresh();
                }
                let input_ms = start.elapsed().as_secs_f64() * 1000.;
                let callbacks = window.simulate_next_frame(cx);
                RENDER_NS.with(|n| n.set(0));
                let start = Instant::now();
                let clear = window.draw(cx);
                let draw_ms = start.elapsed().as_secs_f64() * 1000.;
                let render_ms = RENDER_NS.with(|n| n.get() as f64 / 1e6);
                clear.clear(cx);
                if frame >= 8 {
                    input.push(input_ms);
                    draw.push(draw_ms);
                    reconstruction.push(render_ms);
                    remainder.push((draw_ms - render_ms).max(0.));
                    callback_counts.push(callbacks);
                }
            });
        }
        let renders = cx.update(|_, cx| view.read(cx).render_count) - renders_before;
        if scenario == "scroll" {
            assert!(
                cx.update(|_, cx| view.read(cx).settings_scroll.offset().y) < px(0.),
                "probe must actually scroll overflowing content"
            );
        }
        report(&format!("{scenario} input/dispatch"), &mut input);
        report(&format!("{scenario} CPU draw"), &mut draw);
        report(
            &format!("{scenario} Home reconstruction"),
            &mut reconstruction,
        );
        report(
            &format!("{scenario} remaining CPU layout/prepaint/paint"),
            &mut remainder,
        );
        println!(
            "{scenario}: Home renders={renders}; animation callbacks={}; native presentation intervals=unavailable (TestPlatform)",
            callback_counts.iter().sum::<usize>()
        );
    }
}

#[gpui::test]
fn switch_preferences_preserve_queued_recording_and_round_trip(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            let (request, _) = crate::video::adapter::tests::fixture();
            home.model.request = Some(request.clone());
            let document = frame_settings::model::SettingsDocument::new(request);
            home.save_latest = Some((Some(document.clone()), home.model.preferences.clone()));
            home.model.preferences.filled_buttons = false;
            for checked in [true, false, true, false] {
                home.dispatch(Action::FilledButtons, window, cx);
                let (queued, prefs) = home.save_latest.as_ref().unwrap();
                assert_eq!(
                    serde_json::to_value(queued).unwrap(),
                    serde_json::to_value(&document).unwrap()
                );
                assert_eq!(prefs.filled_buttons, checked);
                let reopened: Preferences =
                    serde_json::from_slice(&serde_json::to_vec(prefs).unwrap()).unwrap();
                assert_eq!(reopened.filled_buttons, checked);
                assert!(home.save_running);
            }
            home.model.closing = true;
            home.dispatch(Action::FilledButtons, window, cx);
            assert!(
                !home.model.preferences.filled_buttons,
                "unavailable setting changed"
            );
        })
    });
}
