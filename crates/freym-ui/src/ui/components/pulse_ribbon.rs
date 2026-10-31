//! Chromatic Pulse: measured per-source input level history, never generated activity.
use crate::ui::theme::{colors::Color, motion};
use frame_contracts::{AudioLevel, AudioSourceId, AudioSourceKind};
use gpui::{
    Background, Context, Hsla, IntoElement, Path, PathBuilder, Pixels, Point, Render, Window,
    canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, rgb, size,
};
use std::{
    cell::Cell,
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant},
};

const HISTORY: usize = 80;
const FRESH: Duration = Duration::from_millis(250);
const PAINT_LEASE: Duration = Duration::from_millis(150);
const SAMPLE: Duration = Duration::from_millis(motion::METER_SAMPLE_MS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RibbonStatus {
    Live,
    Off,
    Muted,
    Unavailable,
    Paused,
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Envelope {
    rms: f32,
    peak: f32,
    valid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChromaticPalette {
    start: Hsla,
    end: Hsla,
}
impl ChromaticPalette {
    fn for_source(kind: AudioSourceKind) -> Self {
        let (start, end) = match kind {
            AudioSourceKind::Microphone => (0xa78bfa, 0xdb8bc9),
            AudioSourceKind::System => (0x52cddd, 0x729ff0),
            AudioSourceKind::Application => (0xe8b477, 0xe68f81),
        };
        Self {
            start: rgb(start).into(),
            end: rgb(end).into(),
        }
    }
    fn gradient(self, alpha: f32) -> Background {
        linear_gradient(
            90.,
            linear_color_stop(self.start.opacity(alpha * 0.72), 0.),
            linear_color_stop(self.end.opacity(alpha), 1.),
        )
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct PulseInk {
    trace: f32,
    fill: f32,
    glow: f32,
}
impl PulseInk {
    fn measured(envelope: Envelope, effects: bool) -> Self {
        let energy = (envelope.rms * 0.78 + envelope.peak * 0.22).clamp(0., 1.);
        if !envelope.valid || energy <= 0.001 {
            return Self::default();
        }
        Self {
            trace: 0.28 + energy * 0.58,
            fill: energy * 0.065,
            glow: if effects { energy * 0.10 } else { 0. },
        }
    }
}

// Both strokes and the fill follow the same bounded, non-overshooting curve.
fn trace_path(
    points: &[Point<Pixels>],
    width: Option<f32>,
    baseline: Pixels,
) -> Option<Path<Pixels>> {
    let (&first, &last) = (points.first()?, points.last()?);
    if points.len() < 2 {
        return None;
    }
    let mut path = width.map_or_else(PathBuilder::fill, |w| PathBuilder::stroke(px(w)));
    path.move_to(first);
    for pair in points.windows(2) {
        let (previous, next) = (pair[0], pair[1]);
        path.curve_to(
            point((previous.x + next.x) / 2., (previous.y + next.y) / 2.),
            previous,
        );
    }
    path.line_to(last);
    if width.is_none() {
        path.line_to(point(last.x, baseline));
        path.line_to(point(first.x, baseline));
        path.close();
    }
    path.build().ok()
}

fn dbfs(value: f32) -> f32 {
    if value == 0. {
        f32::NEG_INFINITY
    } else {
        20. * value.log10()
    }
}
fn height(value: f32) -> f32 {
    ((dbfs(value) + 72.) / 72.).clamp(0., 1.)
}
fn number(value: f32) -> String {
    if value == 0. {
        "−∞".into()
    } else {
        format!("{:.1}", dbfs(value))
    }
}

struct History {
    source: AudioSourceId,
    session: Option<String>,
    status: RibbonStatus,
    points: VecDeque<Envelope>,
    latest: Option<(f32, f32)>,
    received: Option<Instant>,
    clip_until: Option<Instant>,
}
impl History {
    fn new(source: AudioSourceId) -> Self {
        Self {
            source,
            session: None,
            status: RibbonStatus::Live,
            points: VecDeque::from(vec![Envelope::default(); HISTORY]),
            latest: None,
            received: None,
            clip_until: None,
        }
    }
    fn clear(&mut self) -> bool {
        let changed = self.latest.is_some()
            || self.clip_until.is_some()
            || self.points.iter().any(|p| p.valid);
        self.points.make_contiguous().fill(Envelope::default());
        self.latest = None;
        self.received = None;
        self.clip_until = None;
        changed
    }
    fn status(&mut self, status: RibbonStatus) -> bool {
        let changed = self.status != status;
        self.status = status;
        if status != RibbonStatus::Live {
            return self.clear() || changed;
        }
        changed
    }
    fn expire(&mut self, now: Instant) -> bool {
        if self
            .received
            .is_some_and(|at| now.saturating_duration_since(at) > FRESH)
        {
            self.clear()
        } else {
            false
        }
    }
    fn push(&mut self, session: Option<&str>, level: Option<&AudioLevel>, now: Instant) -> bool {
        let mut changed = false;
        if self.session.as_deref() != session {
            changed |= self.clear();
            self.session = session.map(str::to_owned);
        }
        if self.status != RibbonStatus::Live || session.is_none() {
            return self.clear() || changed;
        }
        let Some((rms, peak, clipping)) =
            level.filter(|l| l.source_id == self.source).and_then(|l| {
                let (rms, peak) = (l.rms?, l.peak?);
                (rms.is_finite()
                    && peak.is_finite()
                    && rms >= 0.
                    && peak >= 0.
                    && rms <= peak + 0.000001)
                    .then_some((rms, peak, l.clipping == Some(true) || peak >= 1.))
            })
        else {
            return self.clear() || changed;
        };
        let dt = self
            .received
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or(SAMPLE)
            .as_secs_f32()
            .min(0.25);
        let previous = *self.points.back().unwrap();
        let smooth = |old: f32, target: f32| {
            let tau = if target > old {
                motion::METER_ATTACK_SECONDS
            } else {
                motion::METER_RELEASE_SECONDS
            };
            let value = old + (target - old) * (1. - (-dt / tau).exp());
            if (target - value).abs() < 0.001 {
                target
            } else {
                value
            }
        };
        let next = Envelope {
            rms: smooth(previous.rms, height(rms)),
            peak: smooth(previous.peak, height(peak)),
            valid: true,
        };
        changed |= self.points.iter().any(|p| *p != next) || self.latest != Some((rms, peak));
        self.points.pop_front();
        self.points.push_back(next);
        self.latest = Some((rms, peak));
        self.received = Some(now);
        if clipping {
            self.clip_until = Some(now + Duration::from_secs(1));
            changed = true;
        }
        if self.clip_until.is_some_and(|until| until <= now) {
            self.clip_until = None;
            changed = true;
        }
        changed
    }
    fn display_envelope(&self, now: Instant, reduced: bool) -> Envelope {
        if reduced {
            return self
                .latest
                .map_or(Envelope::default(), |(rms, peak)| Envelope {
                    rms: height(rms),
                    peak: height(peak),
                    valid: true,
                });
        }
        let current = *self.points.back().unwrap();
        let previous = self.points[HISTORY - 2];
        let fraction = self.received.map_or(1., |at| {
            (now.saturating_duration_since(at).as_secs_f32() / SAMPLE.as_secs_f32()).clamp(0., 1.)
        });
        Envelope {
            rms: previous.rms + (current.rms - previous.rms) * fraction,
            peak: previous.peak + (current.peak - previous.peak) * fraction,
            valid: current.valid,
        }
    }
    fn moving(&self, now: Instant) -> bool {
        self.status == RibbonStatus::Live
            && self
                .received
                .is_some_and(|at| now.saturating_duration_since(at) <= FRESH)
            && (self.points.back().unwrap().peak > 0.001
                || self.display_envelope(now, false).peak > 0.001)
            && self.points.iter().any(|p| *p != self.points[0])
    }
    fn text(&self, now: Instant) -> String {
        match self.status {
            RibbonStatus::Off => "Off".into(),
            RibbonStatus::Muted => "Muted".into(),
            RibbonStatus::Unavailable => "Unavailable".into(),
            RibbonStatus::Paused => "Paused".into(),
            RibbonStatus::Live => match self.latest {
                Some((rms, peak)) => format!(
                    "{}P {}  R {} dBFS",
                    if self.clip_until.is_some_and(|t| t > now) {
                        "CLIP · "
                    } else {
                        ""
                    },
                    number(peak),
                    number(rms)
                ),
                None => "No measurement".into(),
            },
        }
    }
}

pub(crate) struct PulseRibbon {
    history: History,
    palette: ChromaticPalette,
    painted: Rc<Cell<Instant>>,
    visible: bool,
    frame_pending: bool,
    paint_active: bool,
    available_width: Option<f32>,
}
impl PulseRibbon {
    pub(crate) fn set_available_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.available_width != Some(width) {
            self.available_width = Some(width);
            cx.notify();
        }
    }
    pub fn new(source: AudioSourceId, kind: AudioSourceKind) -> Self {
        Self {
            history: History::new(source),
            palette: ChromaticPalette::for_source(kind),
            painted: Rc::new(Cell::new(Instant::now())),
            visible: true,
            frame_pending: false,
            paint_active: false,
            available_width: None,
        }
    }
    pub fn set_status(&mut self, status: RibbonStatus, cx: &mut Context<Self>) {
        if self.history.status(status) {
            cx.notify();
        }
    }
    pub fn sample(
        &mut self,
        session: Option<&str>,
        level: Option<&AudioLevel>,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        let changed = self.history.push(session, level, now);
        let painted =
            self.visible && now.saturating_duration_since(self.painted.get()) <= PAINT_LEASE;
        let became_visible = painted && !self.paint_active;
        self.paint_active = painted;
        if painted && (changed || became_visible) {
            cx.notify();
        }
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.paint_active = false;
        if self.history.clear() {
            cx.notify();
        }
    }
    pub fn expire(&mut self, cx: &mut Context<Self>) {
        if self.history.expire(Instant::now()) {
            cx.notify();
        }
    }
    #[cfg(test)]
    pub(crate) fn latest(&self) -> Option<(f32, f32)> {
        self.history.latest
    }
    #[cfg(test)]
    pub(crate) fn has_palette_for(&self, kind: AudioSourceKind) -> bool {
        self.palette == ChromaticPalette::for_source(kind)
    }
}

struct MeterTooltip(String);
impl Render for MeterTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(300.))
            .p(px(8.))
            .rounded(px(6.))
            .bg(Color::PrimarySurface.get())
            .text_size(px(12.))
            .text_color(Color::PrimaryText.get())
            .child(self.0.clone())
    }
}
impl Render for PulseRibbon {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        self.visible = window.is_visible();
        self.history.expire(now);
        let reduced = cx.reduce_motion();
        let moving = self.visible
            && !reduced
            && self.history.moving(now)
            && now.saturating_duration_since(self.painted.get()) <= PAINT_LEASE;
        if moving && !self.frame_pending {
            self.frame_pending = true;
            cx.on_next_frame(window, |this, window, cx| {
                this.frame_pending = false;
                this.visible = window.is_visible();
                let now = Instant::now();
                let expired = this.history.expire(now);
                if this.visible
                    && now.saturating_duration_since(this.painted.get()) <= PAINT_LEASE
                    && (expired || (!cx.reduce_motion() && this.history.moving(now)))
                {
                    cx.notify();
                }
            });
        }
        let text = self.history.text(now);
        let description = format!(
            "Input {} before gain and mute. Peak / RMS: {text}",
            if reduced { "levels" } else { "level history" }
        );
        let clipped = self.history.clip_until.is_some_and(|until| until > now);
        let palette = if crate::ui::theme::ThemeId::current() == crate::ui::theme::ThemeId::White {
            let deepen = |color: Hsla| Hsla {
                l: color.l.min(0.36),
                s: color.s.max(0.60),
                ..color
            };
            ChromaticPalette {
                start: deepen(self.palette.start),
                end: deepen(self.palette.end),
            }
        } else {
            self.palette
        };
        let baseline = Color::SecondaryText.get().opacity(0.18);
        let current = self.history.display_envelope(now, reduced);
        let points: [Envelope; HISTORY] = std::array::from_fn(|i| self.history.points[i]);
        let ink = PulseInk::measured(current, !reduced && !crate::ui::theme::low_cost_effects());
        let peak_highlight = self.history.latest.is_some_and(|(_, peak)| peak >= 0.708);
        let peak_color = if clipped {
            Color::Danger.get()
        } else {
            palette.end
        };
        let fraction = if moving {
            self.history
                .received
                .map(|at| now.saturating_duration_since(at).as_secs_f32() / SAMPLE.as_secs_f32())
                .unwrap_or(0.)
                .clamp(0., 1.)
        } else {
            0.
        };
        let painted = self.painted.clone();
        let show_values = self.available_width.unwrap_or_else(|| {
            super::RecorderGeometry::new(
                crate::ui::viewport(window, cx).width.into(),
                crate::ui::viewport(window, cx).height.into(),
            )
            .audio_width()
        }) >= 640.;
        div()
            .id(format!("pulse-ribbon-{}", self.history.source.0))
            .debug_selector({
                let id = self.history.source.0.clone();
                move || format!("pulse-ribbon-{id}")
            })
            .w_full()
            .h(px(22.))
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .aria_label(description.clone())
            .tooltip(move |_, cx| cx.new(|_| MeterTooltip(description.clone())).into())
            .child(
                div().flex_1().min_w_0().h(px(18.)).overflow_hidden().child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            painted.set(Instant::now());
                            let base_y = bounds.bottom() - px(2.);
                            window.paint_quad(fill(
                                gpui::Bounds::new(
                                    point(bounds.left(), base_y),
                                    size(bounds.size.width, px(1.)),
                                ),
                                baseline,
                            ));
                            if ink.trace == 0. {
                                return;
                            }
                            let step = f32::from(bounds.size.width) / (HISTORY - 1) as f32;
                            let mut peaks = [point(px(0.), px(0.)); HISTORY];
                            let mut rms = peaks;
                            let mut count = 0;
                            // Reset always clears the whole history, so valid samples form a suffix.
                            if reduced {
                                for x in [bounds.left(), bounds.right() - px(1.)] {
                                    peaks[count] = point(x, base_y - px(current.peak * 13.));
                                    rms[count] = point(x, base_y - px(current.rms * 13.));
                                    count += 1;
                                }
                            } else {
                                for (index, entry) in
                                    points.iter().enumerate().filter(|(_, e)| e.valid)
                                {
                                    let x = bounds.left() + px((index as f32 - fraction) * step);
                                    peaks[count] = point(x, base_y - px(entry.peak * 13.));
                                    rms[count] = point(x, base_y - px(entry.rms * 13.));
                                    count += 1;
                                }
                            }
                            if let Some(path) = trace_path(&rms[..count], None, base_y) {
                                window.paint_path(path, palette.gradient(ink.fill));
                            }
                            for (trace, width, alpha) in [
                                (&peaks[..count], 3.5, ink.glow),
                                (&peaks[..count], 1., ink.trace * 0.56),
                                (&rms[..count], 1.5, ink.trace),
                            ] {
                                if alpha > 0.
                                    && let Some(path) = trace_path(trace, Some(width), base_y)
                                {
                                    window.paint_path(path, palette.gradient(alpha));
                                }
                            }
                            if (clipped || peak_highlight)
                                && let Some(last) = peaks[..count].last()
                            {
                                // A small cap marks the measured peak; source colors remain legible.
                                window.paint_quad(fill(
                                    gpui::Bounds::new(
                                        point(
                                            (last.x - px(3.)).max(bounds.left()),
                                            last.y - px(0.5),
                                        ),
                                        size(px(3.), px(1.)),
                                    ),
                                    peak_color.opacity(if clipped {
                                        0.85
                                    } else {
                                        ink.trace * 0.65
                                    }),
                                ));
                            }
                        },
                    )
                    .size_full(),
                ),
            )
            .when(show_values, |d| {
                d.child(
                    div()
                        .w(px(190.))
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .line_height(px(14.))
                        .text_color(if clipped {
                            Color::Danger.get()
                        } else {
                            Color::SecondaryText.get()
                        })
                        .child(text),
                )
            })
            .when(!show_values && clipped, |d| {
                d.child(
                    div()
                        .text_size(px(10.))
                        .text_color(Color::Danger.get())
                        .child("CLIP"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn level(rms: f32, peak: f32) -> AudioLevel {
        AudioLevel {
            source_id: AudioSourceId("mic".into()),
            rms: Some(rms),
            peak: Some(peak),
            clipping: Some(peak >= 1.),
        }
    }
    #[test]
    fn input_dbfs_and_silence_come_from_the_pcm_measurement_without_smoothing_the_readout() {
        let hub = frame_audio::metering::MeterHub::default();
        let id = AudioSourceId("mic".into());
        hub.begin("one".into(), std::slice::from_ref(&id));
        hub.observe("one", &id, &[0.5, -0.5, 0., 0.]);
        let data = hub.snapshot();
        let mut history = History::new(id.clone());
        let now = Instant::now();
        history.push(data.session_id.as_deref(), data.sources.first(), now);
        assert_eq!(history.latest, Some(((0.125f32).sqrt(), 0.5)));
        assert!((dbfs(0.5) + 6.0206).abs() < 0.0001);
        assert!((dbfs((0.125f32).sqrt()) + 9.0309).abs() < 0.0001);
        assert!(history.text(now).contains("P -6.0  R -9.0 dBFS"));
        assert!(
            history.points.back().unwrap().peak < height(0.5),
            "Visual attack is separate from raw numeric levels"
        );
        hub.begin("silence".into(), std::slice::from_ref(&id));
        hub.observe("silence", &id, &[0., 0.]);
        let data = hub.snapshot();
        history.push(
            data.session_id.as_deref(),
            data.sources.first(),
            now + SAMPLE,
        );
        assert!(history.text(now + SAMPLE).contains("P −∞  R −∞"));
        assert!(!history.moving(now + SAMPLE));
    }
    #[test]
    fn off_mute_missing_invalid_foreign_and_stale_measurements_clear_activity_and_clipping() {
        let now = Instant::now();
        for status in [
            RibbonStatus::Off,
            RibbonStatus::Muted,
            RibbonStatus::Unavailable,
            RibbonStatus::Paused,
        ] {
            let mut history = History::new(AudioSourceId("mic".into()));
            history.push(Some("one"), Some(&level(0.7, 1.2)), now);
            assert!(history.clip_until.is_some());
            history.status(status);
            history.push(Some("one"), Some(&level(0.7, 1.2)), now + SAMPLE);
            assert!(history.latest.is_none() && history.clip_until.is_none());
            assert!(!history.moving(now + SAMPLE));
        }
        let mut history = History::new(AudioSourceId("mic".into()));
        for invalid in [
            None,
            Some(level(f32::NAN, 0.5)),
            Some(level(-1., 0.5)),
            Some(level(1., 0.5)),
            Some(AudioLevel {
                source_id: AudioSourceId("other".into()),
                ..level(0.5, 0.5)
            }),
        ] {
            history.push(Some("one"), Some(&level(0.7, 1.1)), now);
            history.push(Some("one"), invalid.as_ref(), now + SAMPLE);
            assert!(history.latest.is_none() && history.points.iter().all(|p| !p.valid));
        }
        history.push(Some("one"), Some(&level(0.7, 1.1)), now);
        assert!(history.expire(now + FRESH + SAMPLE));
        assert!(history.latest.is_none() && history.clip_until.is_none());
    }
    #[test]
    fn history_is_bounded_and_session_changes_cannot_replay_the_previous_source_trace() {
        let now = Instant::now();
        let mut history = History::new(AudioSourceId("mic".into()));
        for index in 0..1000 {
            let value = if index % 2 == 0 { 0.5 } else { 0.1 };
            history.push(
                Some("one"),
                Some(&level(value, value)),
                now + SAMPLE * index,
            );
        }
        assert_eq!(history.points.len(), HISTORY);
        assert!(
            history
                .points
                .iter()
                .all(|p| (0. ..=1.).contains(&p.peak) && p.rms <= p.peak)
        );
        history.push(Some("two"), Some(&level(0.1, 0.1)), now + SAMPLE * 1001);
        assert_eq!(history.points.iter().filter(|p| p.valid).count(), 1);
        history.push(None, Some(&level(0.1, 0.1)), now + SAMPLE * 1002);
        assert!(history.latest.is_none());
    }
    #[test]
    fn chromatic_attack_release_tracks_levels_and_settles_to_neutral_silence() {
        let now = Instant::now();
        let mut history = History::new(AudioSourceId("mic".into()));
        assert_eq!(
            PulseInk::measured(history.display_envelope(now, false), true),
            PulseInk::default()
        );
        history.push(Some("one"), Some(&level(0.7, 0.95)), now);
        let attack_start = history.display_envelope(now, false);
        let attack_mid = history.display_envelope(now + SAMPLE / 2, false);
        let attack_end = history.display_envelope(now + SAMPLE, false);
        assert!(attack_start.rms < attack_mid.rms && attack_mid.rms < attack_end.rms);
        let quiet = PulseInk::measured(attack_mid, true);
        let loud = PulseInk::measured(attack_end, true);
        assert!(quiet.trace < loud.trace && quiet.fill < loud.fill && quiet.glow < loud.glow);
        assert!(loud.fill <= 0.065 && loud.glow <= 0.10 && loud.trace <= 0.86);
        let no_effects = PulseInk::measured(attack_end, false);
        assert_eq!(no_effects.glow, 0.);
        assert_eq!((no_effects.trace, no_effects.fill), (loud.trace, loud.fill));

        history.push(Some("one"), Some(&level(0., 0.)), now + SAMPLE);
        let release = history.display_envelope(now + SAMPLE * 2, false);
        assert!(release.rms > 0. && release.rms < attack_end.rms);
        assert!(
            release.rms / attack_end.rms > 1. - attack_end.rms / height(0.7),
            "Release retains more energy than the faster attack leaves behind"
        );
        assert_eq!(history.latest, Some((0., 0.)));
        for index in 2..32 {
            history.push(Some("one"), Some(&level(0., 0.)), now + SAMPLE * index);
        }
        let silent = now + SAMPLE * 32;
        assert!(history.points.iter().any(|p| p.peak > 0.));
        assert_eq!(
            PulseInk::measured(history.display_envelope(silent, false), true),
            PulseInk::default()
        );
        assert!(
            !history.moving(silent),
            "Invisible old history must not keep frames running"
        );
        for index in 32..120 {
            history.push(Some("one"), Some(&level(0., 0.)), now + SAMPLE * index);
        }
        let settled = now + SAMPLE * 120;
        assert!(history.points.iter().all(|p| p.peak == 0. && p.rms == 0.));
        assert_eq!(
            PulseInk::measured(history.display_envelope(settled, false), true),
            PulseInk::default()
        );
        assert!(!history.moving(settled));
    }
    #[test]
    fn reduced_motion_uses_current_measurements_without_interpolated_activity() {
        let now = Instant::now();
        let mut history = History::new(AudioSourceId("mic".into()));
        history.push(Some("one"), Some(&level(0.25, 0.5)), now);
        let expected = Envelope {
            rms: height(0.25),
            peak: height(0.5),
            valid: true,
        };
        assert_eq!(history.display_envelope(now, true), expected);
        assert_eq!(history.display_envelope(now + SAMPLE / 2, true), expected);
        history.push(Some("one"), Some(&level(0., 0.)), now + SAMPLE);
        assert_eq!(
            PulseInk::measured(history.display_envelope(now + SAMPLE, true), false),
            PulseInk::default()
        );
    }
    #[test]
    fn source_gradients_are_distinct_and_clipping_preserves_raw_readings_and_hold() {
        let kinds = [
            AudioSourceKind::Microphone,
            AudioSourceKind::System,
            AudioSourceKind::Application,
        ];
        for (index, kind) in kinds.iter().enumerate() {
            let palette = ChromaticPalette::for_source(*kind);
            assert_ne!(palette.start, palette.end);
            for other in &kinds[index + 1..] {
                assert_ne!(palette, ChromaticPalette::for_source(*other));
            }
        }
        let now = Instant::now();
        let mut history = History::new(AudioSourceId("mic".into()));
        let flagged = AudioLevel {
            clipping: Some(true),
            ..level(0.4, 0.8)
        };
        history.push(Some("one"), Some(&flagged), now);
        assert_eq!(history.latest, Some((0.4, 0.8)));
        assert!(history.text(now).starts_with("CLIP · P -1.9  R -8.0"));
        for index in 1..=26 {
            history.push(Some("one"), Some(&level(0.2, 0.4)), now + SAMPLE * index);
        }
        assert!(!history.text(now + SAMPLE * 26).contains("CLIP"));
        history.push(Some("one"), Some(&level(0.9, 1.2)), now + SAMPLE * 27);
        assert!(history.text(now + SAMPLE * 27).contains("CLIP · P 1.6"));
    }
    #[gpui::test]
    fn chromatic_sources_render_silent_active_clipped_and_inactive_in_the_existing_row(
        cx: &mut gpui::TestAppContext,
    ) {
        for kind in [
            AudioSourceKind::Microphone,
            AudioSourceKind::System,
            AudioSourceKind::Application,
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| {
                cx.set_reduce_motion(true);
                PulseRibbon::new(AudioSourceId("mic".into()), kind)
            });
            cx.run_until_parked();
            let bounds = cx.debug_bounds("pulse-ribbon-mic").unwrap();
            assert_eq!(bounds.size.height, px(22.));
            for (rms, peak) in [(0., 0.), (0.25, 0.5), (0.8, 1.1)] {
                view.update(cx, |r, cx| {
                    r.sample(Some("one"), Some(&level(rms, peak)), cx)
                });
                cx.run_until_parked();
                assert_eq!(view.read_with(cx, |r, _| r.latest()), Some((rms, peak)));
                assert!(view.read_with(cx, |r, _| r.has_palette_for(kind)));
                assert_eq!(cx.debug_bounds("pulse-ribbon-mic").unwrap(), bounds);
                assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
            }
            for status in [
                RibbonStatus::Off,
                RibbonStatus::Muted,
                RibbonStatus::Unavailable,
                RibbonStatus::Paused,
            ] {
                view.update(cx, |r, cx| r.set_status(status, cx));
                cx.run_until_parked();
                assert_eq!(view.read_with(cx, |r, _| r.latest()), None);
                assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
            }
        }
    }
    #[gpui::test]
    fn idle_reduced_hidden_and_unpainted_ribbons_do_not_keep_a_frame_loop(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| {
            PulseRibbon::new(AudioSourceId("mic".into()), AudioSourceKind::Microphone)
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
        view.update(cx, |ribbon, cx| {
            ribbon.sample(Some("one"), Some(&level(0.5, 0.5)), cx)
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |r, _| r.frame_pending));
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.run_until_parked();
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        assert!(!view.read_with(cx, |r, _| r.frame_pending));
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
        cx.update(|_, cx| cx.set_reduce_motion(false));
        view.update(cx, |r, cx| {
            r.painted.set(Instant::now() - Duration::from_secs(1));
            r.sample(Some("one"), Some(&level(0.1, 0.1)), cx);
        });
        // A frame queued before the view left the viewport may drain once.
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        assert!(!view.read_with(cx, |r, _| r.frame_pending));
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
        cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
        view.update(cx, |r, cx| {
            r.clear(cx);
            r.set_status(RibbonStatus::Off, cx);
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
    }
}
