//! Compact current-process status strip, independent of the media tasks.
use crate::ui::theme::colors::Color;
use freym_platform::resources::{Sampler, Usage};
use gpui::{Context, IntoElement, Render, Window, div, prelude::*, px};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub(crate) struct ResourceUsage {
    usage: Usage,
    active: bool,
    history: MemoryHistory,
    report: UsageReport,
    _task: gpui::Task<()>,
}
impl ResourceUsage {
    pub(crate) fn recording(&mut self, recording: bool, cx: &mut Context<Self>) {
        if self.history.recording(recording, Instant::now()) {
            if recording {
                self.report = Default::default();
            }
            cx.notify();
        }
    }
    pub(crate) fn set_active(&mut self, active: bool) {
        self.active = active;
    }
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The GPUI task owns the sampler; all OS calls run on background workers.
        // Dropping Home drops the entity/task and closes the query, with no thread
        // or media-controller lifetime added to the application.
        let task = cx.spawn_in(window, async move |view, cx| {
            let mut sampler = Sampler::default();
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let recording_start = match view.update_in(cx, |this, window, _| {
                    (
                        this.history.running || (this.active && window.is_visible()),
                        this.history.start,
                    )
                }) {
                    Ok((true, start)) => start,
                    Ok((false, _)) => continue,
                    Err(_) => break,
                };
                let (next, usage) = cx
                    .background_executor()
                    .spawn(async move {
                        let usage = sampler.sample();
                        (sampler, usage)
                    })
                    .await;
                sampler = next;
                if view
                    .update_in(cx, |this, window, cx| {
                        let changed = values(this.usage) != values(usage);
                        this.usage = usage;
                        let sampled = this.history.start == recording_start
                            && this.history.sample(usage.ram_mb, Instant::now());
                        if sampled {
                            this.report.sample(usage);
                        }
                        if this.active && window.is_visible() && (changed || sampled) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            usage: Usage::default(),
            active: true,
            history: MemoryHistory::default(),
            report: Default::default(),
            _task: task,
        }
    }
}
pub(crate) const HEIGHT: f32 = 42.;

impl ResourceUsage {
    #[allow(dead_code)]
    pub(crate) fn report(&self) -> String {
        self.report.caption()
    }
}
#[derive(Default)]
struct UsageReport {
    cpu: (u64, u64),
    gpu: (u64, u64),
    peak: Option<u64>,
    samples: u64,
}
impl UsageReport {
    fn sample(&mut self, u: Usage) {
        self.samples += 1;
        if let Some(v) = u.cpu {
            self.cpu.0 += u64::from(v);
            self.cpu.1 += 1;
        }
        if let Some(v) = u.gpu {
            self.gpu.0 += u64::from(v);
            self.gpu.1 += 1;
        }
        if let Some(v) = u.ram_mb {
            self.peak = Some(self.peak.unwrap_or(0).max(v));
        }
    }
    #[allow(dead_code)]
    fn caption(&self) -> String {
        let avg = |(s, n): (u64, u64), scale: f64| {
            if n > 0 {
                format!("{:.1}%", s as f64 / n as f64 / scale)
            } else {
                "unmeasured".into()
            }
        };
        format!(
            "Process CPU avg {} · GPU avg {} · RAM peak {} · {} samples at 1 Hz",
            avg(self.cpu, 10.),
            avg(self.gpu, 1.),
            self.peak
                .map_or_else(|| "unmeasured".into(), |n| format!("{n} MB")),
            self.samples
        )
    }
}

/// At most five minutes at the existing one-second sampling cadence. Missing
/// measurements remain gaps, never fabricated zeroes or interpolated samples.
#[derive(Default)]
struct MemoryHistory {
    running: bool,
    start: Option<Instant>,
    samples: VecDeque<(f32, Option<u64>)>,
}
impl MemoryHistory {
    fn recording(&mut self, running: bool, now: Instant) -> bool {
        if running == self.running {
            return false;
        }
        self.running = running;
        self.start = running.then_some(now);
        self.samples.clear();
        true
    }
    fn measured(&self) -> bool {
        self.running && self.samples.iter().any(|sample| sample.1.is_some())
    }
    fn sample(&mut self, ram: Option<u64>, now: Instant) -> bool {
        if !self.running {
            return false;
        }
        let seconds = now.duration_since(self.start.unwrap()).as_secs_f32();
        if self.samples.len() == 300 {
            self.samples.pop_front();
        }
        self.samples.push_back((seconds, ram));
        true
    }
}

pub(crate) struct MemoryGraph {
    source: gpui::Entity<ResourceUsage>,
    _subscription: gpui::Subscription,
}
impl MemoryGraph {
    pub(crate) fn new(source: gpui::Entity<ResourceUsage>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&source, |_, _, cx| cx.notify());
        Self {
            source,
            _subscription: subscription,
        }
    }
}
pub(crate) fn empty_graph() -> gpui::AnyElement {
    memory_graph(&MemoryHistory::default())
}

fn memory_graph(history: &MemoryHistory) -> gpui::AnyElement {
    if !history.measured() {
        return div().into_any_element();
    }
    let samples = history.samples.clone();
    let from = samples.front().map_or(0., |p| p.0);
    let elapsed = samples.back().map_or(0., |p| p.0);
    let to = elapsed.max(from + 1.);
    let peak = samples.iter().filter_map(|p| p.1).max().unwrap_or(0);
    let latest = samples
        .back()
        .and_then(|p| p.1)
        .map_or_else(|| "—".into(), |v| format!("{v} MB"));
    div()
        .id("recording-memory-graph")
        .debug_selector(|| "recording-memory-graph".into())
        .w_full()
        .h(px(76.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(4.))
        .rounded(px(6.))
        .px(px(8.))
        .py(px(6.))
        .bg(Color::RowSurface.get().opacity(0.18))
        .aria_label("FREYM resident memory over recording time; sampled once per second")
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(11.))
                .text_color(Color::SecondaryText.get())
                .child("Resident memory")
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Color::PrimaryText.get())
                        .child(latest),
                ),
        )
        .child(
            div().w_full().min_h_0().flex_1().flex().child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        use gpui::{PathBuilder, point};
                        let x = |s: f32| {
                            bounds.left()
                                + px(2.)
                                + (bounds.size.width - px(4.)).max(px(0.))
                                    * ((s - from) / (to - from))
                        };
                        let y = |v: u64| {
                            bounds.bottom()
                                - px(2.)
                                - (bounds.size.height - px(4.)).max(px(0.))
                                    * (v as f32 / peak.max(1) as f32)
                        };
                        let mut grid = PathBuilder::stroke(px(1.));
                        for fraction in [0., 1.] {
                            let h = bounds.top() + bounds.size.height * fraction;
                            grid.move_to(point(bounds.left(), h));
                            grid.line_to(point(bounds.right(), h));
                        }
                        if let Ok(path) = grid.build() {
                            window.paint_path(path, Color::SubtleBorder.get().opacity(0.25));
                        }
                        let mut path = PathBuilder::stroke(px(1.35));
                        let mut connected = false;
                        for (time, ram) in &samples {
                            if let Some(ram) = ram {
                                let p = point(x(*time), y(*ram));
                                if connected {
                                    path.line_to(p);
                                } else {
                                    path.move_to(p);
                                }
                                connected = true;
                            } else {
                                connected = false;
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, Color::Accent.get().opacity(0.85));
                        }
                        if let Some((time, Some(ram))) = samples.back() {
                            let p = point(x(*time), y(*ram));
                            window.paint_quad(
                                gpui::fill(
                                    gpui::Bounds::new(
                                        point(p.x - px(1.5), p.y - px(1.5)),
                                        gpui::size(px(3.), px(3.)),
                                    ),
                                    Color::Accent.get(),
                                )
                                .corner_radii(px(1.5)),
                            );
                        }
                    },
                )
                .h_full()
                .flex_1()
                .min_w_0(),
            ),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(10.))
                .text_color(Color::SecondaryText.get())
                .child(format!("{:.0}:{:02.0}", (from / 60.).floor(), from % 60.))
                .child(format!("Peak {peak} MB"))
                .child(format!(
                    "{:.0}:{:02.0}",
                    (elapsed / 60.).floor(),
                    elapsed % 60.
                )),
        )
        .into_any_element()
}
impl Render for MemoryGraph {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        memory_graph(&self.source.read(cx).history)
    }
}

fn values(usage: Usage) -> [String; 4] {
    [
        usage
            .cpu
            .map(|v| format!("{:.1}%", v as f32 / 10.))
            .unwrap_or_else(|| "—".into()),
        usage
            .gpu
            .map(|v| format!("{v}%"))
            .unwrap_or_else(|| "—".into()),
        usage
            .ram_mb
            .map(|v| {
                if v >= 1024 {
                    format!("{:.1} GB", v as f64 / 1024.)
                } else {
                    format!("{v} MB")
                }
            })
            .unwrap_or_else(|| "—".into()),
        usage
            .io_bytes_per_sec
            .map(|v| {
                if v >= 1024 * 1024 {
                    format!("{:.1} MB/s", v as f64 / (1024. * 1024.))
                } else {
                    format!("{:.0} KB/s", v as f64 / 1024.)
                }
            })
            .unwrap_or_else(|| "—".into()),
    ]
}
pub(crate) fn strip(usage: Usage) -> impl IntoElement {
    let values = values(usage);
    let description = format!(
        "FREYM process · CPU {} · GPU {} · Memory {} · I/O {}. A dash means unavailable.",
        values[0], values[1], values[2], values[3]
    );
    div()
        .id("resource-usage")
        .debug_selector(|| "resource-usage".into())
        .w_full()
        .h(px(HEIGHT))
        .flex_shrink_0()
        .grid()
        .grid_cols(4)
        // The dock owns the surface; a second fill makes this strip uneven.
        .py(px(3.))
        .aria_label(description)
        .children(
            [
                ("cpu", "CPU"),
                ("gpu", "GPU"),
                ("ram", "RAM"),
                ("disk", "Disk"),
            ]
            .into_iter()
            .zip(values)
            .enumerate()
            .map(|(index, ((key, label), value))| {
                div()
                    .id(format!("resource-{key}"))
                    .debug_selector(move || format!("resource-{key}"))
                    .min_w_0()
                    .h_full()
                    .px(px(5.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.))
                    .when(index > 0, |d| {
                        d.border_l_1().border_color(Color::SubtleBorder.get())
                    })
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(Color::MutedText.get())
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Color::PrimaryText.get())
                            .truncate()
                            .child(value),
                    )
            }),
        )
}
impl Render for ResourceUsage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        strip(self.usage)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_resource_report_averages_only_measured_samples_and_keeps_missing_values() {
        let mut r = UsageReport::default();
        assert!(r.caption().contains("unmeasured"));
        r.sample(Usage {
            cpu: Some(100),
            gpu: Some(0),
            ram_mb: Some(50),
            ..Default::default()
        });
        r.sample(Usage::default());
        r.sample(Usage {
            cpu: Some(200),
            gpu: Some(20),
            ram_mb: Some(45),
            ..Default::default()
        });
        let c = r.caption();
        assert!(
            c.contains("15.0%")
                && c.contains("10.0%")
                && c.contains("50 MB")
                && c.contains("3 samples")
        );
    }
    #[test]
    fn recording_memory_history_is_bounded_retains_gaps_and_restarts() {
        let mut history = MemoryHistory::default();
        let now = Instant::now();
        assert!(!history.sample(Some(500), now));
        assert!(history.recording(true, now));
        assert!(!history.measured());
        assert!(history.sample(None, now));
        assert!(!history.measured());
        history.samples.clear();
        for second in 0..305 {
            assert!(history.sample(
                if second == 100 { None } else { Some(500) },
                now + Duration::from_secs(second)
            ));
        }
        assert_eq!(history.samples.len(), 300);
        assert!(history.measured());
        assert_eq!(history.samples.front().unwrap().0, 5.);
        assert!(history.samples.iter().any(|p| p.1.is_none()));
        assert!(!history.recording(true, now)); // Re-render/paused session keeps points.
        assert!(history.recording(false, now));
        assert!(!history.sample(Some(900), now));
        assert!(history.samples.is_empty());
        assert!(history.start.is_none());
        assert!(!history.measured());
        assert!(history.recording(true, now));
        assert!(history.samples.is_empty());
    }
    #[test]
    fn process_values_never_substitute_machine_load_or_unavailable_zero() {
        assert_eq!(values(Usage::default()), ["—", "—", "—", "—"]);
        assert_eq!(
            values(Usage {
                cpu: Some(15),
                gpu: Some(0),
                ram_mb: Some(512),
                io_bytes_per_sec: Some(1536 * 1024),
            }),
            ["1.5%", "0%", "512 MB", "1.5 MB/s"]
        );
        assert_eq!(
            values(Usage {
                io_bytes_per_sec: Some(0),
                ..Default::default()
            })[3],
            "0 KB/s"
        );
        assert_eq!(
            values(Usage {
                ram_mb: Some(1536),
                ..Default::default()
            })[2],
            "1.5 GB"
        );
    }
}
