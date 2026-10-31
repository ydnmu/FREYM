//! Single-line rolling text, adapted from Zeron (MIT), Copyright (c) 2026 Wing.
//! Source, exact revision, modifications and license: third_party/zeron-rolling-text/.
//! GPUI 0.2.2 has no transformed/blurred glyph API: scale uses font size and
//! blur uses a small Gaussian sample kernel. No renderer changes or pulse clock.
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, Entity, Font, FontId, GlobalElementId, GlyphId,
    InspectorElementId, IntoElement, LayoutId, Pixels, RenderOnce, ShapedLine, SharedString, Style,
    TextRun, Window, div, point, prelude::*, px,
};
use std::time::Instant;

const DURATION: f32 = 0.550;
const RESIZE: f32 = 0.200;
const STAGGER: f32 = 0.3;
const TRAVEL: f32 = 0.35;
const SCALE: f32 = 0.6;
const BLUR: f32 = 0.1;

/// Caller supplies a stable ID independent of the displayed value. Typography,
/// color and opacity inherit from the surrounding control. First paint is still.
#[derive(IntoElement)]
pub struct RollingText {
    id: ElementId,
    text: SharedString,
}
impl RollingText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
        }
    }
}
impl RenderOnce for RollingText {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(format!("rolling-state-{}", self.id), cx, |_, _| {
            RollState::default()
        });
        div().id(self.id).min_w_0().child(RollLine {
            state,
            text: self.text,
        })
    }
}

// Solve CSS cubic-bezier x(t), rather than treating its parameter as time.
fn ease(x: f32, x1: f32, x2: f32, y1: f32, y2: f32) -> f32 {
    if x <= 0. {
        return 0.;
    }
    if x >= 1. {
        return 1.;
    }
    let at = |t: f32, a: f32, b: f32| {
        3. * (1. - t).powi(2) * t * a + 3. * (1. - t) * t * t * b + t.powi(3)
    };
    let (mut lo, mut hi) = (0., 1.);
    for _ in 0..20 {
        let t = (lo + hi) / 2.;
        if at(t, x1, x2) < x {
            lo = t;
        } else {
            hi = t;
        }
    }
    at((lo + hi) / 2., y1, y2)
}
fn glide(x: f32) -> f32 {
    ease(x, 0.22, 0.36, 1., 1.)
}
fn resize(x: f32) -> f32 {
    ease(x, 0., 0.58, 0., 1.)
}
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Byte lengths of non-overlapping unchanged character runs, as in Zeron.
fn shared_ends(from: &str, to: &str) -> (usize, usize) {
    let prefix: usize = from
        .chars()
        .zip(to.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let suffix = from[prefix..]
        .chars()
        .rev()
        .zip(to[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    (prefix, suffix)
}

#[derive(Clone, Debug)]
struct Glyph {
    font: FontId,
    id: GlyphId,
    index: usize,
    x: f32,
    y: f32,
    advance: f32,
    emoji: bool,
}
fn glyphs(line: &ShapedLine) -> Vec<Glyph> {
    let mut glyphs = Vec::new();
    for run in &line.runs {
        for g in &run.glyphs {
            glyphs.push(Glyph {
                font: run.font_id,
                id: g.id,
                index: g.index,
                x: g.position.x.into(),
                y: g.position.y.into(),
                advance: 0.,
                emoji: g.is_emoji,
            });
        }
    }
    for i in 0..glyphs.len() {
        let end = glyphs.get(i + 1).map_or(line.width.into(), |g| g.x);
        glyphs[i].advance = (end - glyphs[i].x).max(0.);
    }
    glyphs
}
fn delay(glyphs: &[Glyph], index: usize, prefix: usize, suffix_start: usize) -> f32 {
    let changed: Vec<_> = glyphs
        .iter()
        .filter(|g| g.index >= prefix && g.index < suffix_start)
        .collect();
    let min = changed.iter().map(|g| g.x).fold(f32::INFINITY, f32::min);
    let max = changed
        .iter()
        .map(|g| g.x)
        .fold(f32::NEG_INFINITY, f32::max);
    if max <= min {
        return 0.;
    }
    let count = changed.len();
    DURATION * STAGGER * count.saturating_sub(1) as f32 / count.max(1) as f32
        * ((glyphs[index].x - min) / (max - min)).clamp(0., 1.)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pose {
    x: f32,
    y: f32,
    scale: f32,
    alpha: f32,
}
impl Pose {
    fn rest(g: &Glyph) -> Self {
        Self {
            x: g.x,
            y: 0.,
            scale: 1.,
            alpha: 1.,
        }
    }
    fn mix(self, to: Self, t: f32) -> Self {
        Self {
            x: lerp(self.x, to.x, t),
            y: lerp(self.y, to.y, t),
            scale: lerp(self.scale, to.scale, t),
            alpha: lerp(self.alpha, to.alpha, t),
        }
    }
}
#[derive(Clone)]
struct Layer {
    glyph: Glyph,
    from: Pose,
    to: Pose,
    started: Instant,
    delay: f32,
    duration: f32,
    current: bool,
    layout: bool,
}
impl Layer {
    fn sample(&self, now: Instant) -> Pose {
        let t = (now.saturating_duration_since(self.started).as_secs_f32() - self.delay)
            / self.duration.max(f32::EPSILON);
        self.from
            .mix(self.to, if self.layout { resize(t) } else { glide(t) })
    }
    fn active(&self, now: Instant) -> bool {
        self.from != self.to
            && now.saturating_duration_since(self.started).as_secs_f32()
                < self.delay + self.duration
    }
    fn still(glyph: Glyph, now: Instant) -> Self {
        let pose = Pose::rest(&glyph);
        Self {
            glyph,
            from: pose,
            to: pose,
            started: now,
            delay: 0.,
            duration: 0.,
            current: true,
            layout: false,
        }
    }
}
#[derive(Default)]
struct RollState {
    text: SharedString,
    font: Option<Font>,
    font_size: Pixels,
    layers: Vec<Layer>,
    width_from: f32,
    width_to: f32,
    started: Option<Instant>,
}
impl RollState {
    fn width(&self, now: Instant) -> f32 {
        let elapsed = self
            .started
            .map_or(RESIZE, |t| now.saturating_duration_since(t).as_secs_f32());
        lerp(self.width_from, self.width_to, resize(elapsed / RESIZE))
    }
    fn active(&self, now: Instant) -> bool {
        self.layers.iter().any(|g| g.active(now))
            || (self.width_from != self.width_to
                && self
                    .started
                    .is_some_and(|t| now.saturating_duration_since(t).as_secs_f32() < RESIZE))
    }
    fn update(
        &mut self,
        text: SharedString,
        line: &ShapedLine,
        font: Font,
        font_size: Pixels,
        now: Instant,
        reduced: bool,
    ) {
        if reduced || self.font.as_ref() != Some(&font) || self.font_size != font_size {
            self.layers = glyphs(line)
                .into_iter()
                .map(|g| Layer::still(g, now))
                .collect();
            self.text = text;
            self.font = Some(font);
            self.font_size = font_size;
            self.width_from = f32::from(line.width).ceil();
            self.width_to = self.width_from;
            self.started = None;
            return;
        }
        self.layers.retain(|g| g.current || g.active(now));
        if self.text == text {
            return;
        }
        let (prefix, suffix) = shared_ends(&self.text, &text);
        let from_len = self.text.len();
        let to_len = text.len();
        let targets = glyphs(line);
        let old_glyphs: Vec<_> = self
            .layers
            .iter()
            .filter(|g| g.current)
            .map(|g| g.glyph.clone())
            .collect();
        let mut claimed = vec![false; targets.len()];
        let mut layers = Vec::new();
        let travel = f32::from(font_size) * TRAVEL;
        for layer in &self.layers {
            if !layer.current {
                layers.push(layer.clone());
                continue;
            }
            let pose = layer.sample(now);
            let index = layer.glyph.index;
            let shared = if index < prefix {
                Some(index)
            } else if index >= from_len - suffix {
                Some(to_len - (from_len - index))
            } else {
                None
            };
            let target = shared.and_then(|index| {
                targets.iter().enumerate().find(|(i, g)| {
                    !claimed[*i]
                        && g.index == index
                        && g.font == layer.glyph.font
                        && g.id == layer.glyph.id
                })
            });
            if let Some((i, g)) = target {
                claimed[i] = true;
                layers.push(Layer {
                    glyph: g.clone(),
                    from: pose,
                    to: Pose::rest(g),
                    started: now,
                    delay: 0.,
                    duration: if pose.scale == 1. && pose.alpha == 1. {
                        RESIZE
                    } else {
                        DURATION
                    },
                    current: true,
                    layout: pose.scale == 1. && pose.alpha == 1.,
                });
            } else if pose.alpha > 0.001 {
                let i = old_glyphs
                    .iter()
                    .position(|g| g.index == index && g.id == layer.glyph.id)
                    .unwrap_or(0);
                layers.push(Layer {
                    glyph: layer.glyph.clone(),
                    from: pose,
                    to: Pose {
                        y: -travel,
                        scale: SCALE,
                        alpha: 0.,
                        ..pose
                    },
                    started: now,
                    delay: delay(&old_glyphs, i, prefix, from_len - suffix),
                    duration: DURATION,
                    current: false,
                    layout: false,
                });
            }
        }
        for (i, g) in targets.iter().enumerate() {
            if claimed[i] {
                continue;
            }
            // Reclaim a visible outgoing glyph when a rapid change reverses.
            let returning = layers.iter().position(|l| {
                !l.current
                    && l.glyph.index == g.index
                    && l.glyph.id == g.id
                    && l.glyph.font == g.font
            });
            let from = returning
                .map(|i| layers.remove(i).sample(now))
                .unwrap_or(Pose {
                    y: travel,
                    scale: SCALE,
                    alpha: 0.,
                    ..Pose::rest(g)
                });
            layers.push(Layer {
                glyph: g.clone(),
                from,
                to: Pose::rest(g),
                started: now,
                delay: delay(&targets, i, prefix, to_len - suffix),
                duration: DURATION,
                current: true,
                layout: false,
            });
        }
        self.width_from = self.width(now);
        self.width_to = f32::from(line.width).ceil();
        self.started = Some(now);
        self.text = text;
        self.layers = layers;
    }
}

struct RollLine {
    state: Entity<RollState>,
    text: SharedString,
}
struct Frame {
    line: ShapedLine,
    size: Pixels,
    height: Pixels,
    layers: Vec<(Glyph, Pose)>,
    active: bool,
}
impl IntoElement for RollLine {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for RollLine {
    type RequestLayoutState = Frame;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Frame) {
        let style = window.text_style();
        let font = style.font();
        let size = style.font_size.to_pixels(window.rem_size());
        let height = window.pixel_snap(style.line_height.to_pixels(size.into(), window.rem_size()));
        let line = window.text_system().shape_line(
            self.text.clone(),
            size,
            &[TextRun {
                len: self.text.len(),
                font: font.clone(),
                color: style.color,
                background_color: None,
                underline: style.underline,
                strikethrough: style.strikethrough,
            }],
            None,
        );
        let reduced = cx.reduce_motion();
        let now = Instant::now();
        let (width, layers, active) = self.state.update(cx, |state, _| {
            state.update(self.text.clone(), &line, font, size, now, reduced);
            (
                state.width(now),
                state
                    .layers
                    .iter()
                    .map(|l| (l.glyph.clone(), l.sample(now)))
                    .collect(),
                state.active(now),
            )
        });
        let mut layout = Style::default();
        layout.size.width = px(width).into();
        layout.size.height = height.into();
        layout.min_size.width = px(0.).into();
        layout.flex_shrink = 1.;
        (
            window.request_layout(layout, [], cx),
            Frame {
                line,
                size,
                height,
                layers,
                active,
            },
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame: &mut Frame,
        window: &mut Window,
        _: &mut App,
    ) {
        if !frame.active && frame.line.width > bounds.size.width {
            let style = window.text_style();
            let font = style.font();
            let runs = [TextRun {
                len: self.text.len(),
                font: font.clone(),
                color: style.color,
                background_color: None,
                underline: style.underline,
                strikethrough: style.strikethrough,
            }];
            let (text, runs) = window
                .text_system()
                .line_wrapper(font, frame.size)
                .truncate_line(
                    self.text.clone(),
                    bounds.size.width,
                    "…",
                    &runs,
                    gpui::TruncateFrom::End,
                );
            frame.line = window
                .text_system()
                .shape_line(text, frame.size, &runs, None);
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame: &mut Frame,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let travel = frame.size * TRAVEL;
        let halo = frame.size * BLUR * 3.;
        let mask = Bounds {
            origin: point(bounds.left() - halo, bounds.top() - travel * 2.),
            size: gpui::size(
                bounds.size.width + halo * 2.,
                bounds.size.height + travel * 4.,
            ),
        };
        window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
            if !frame.active {
                let _ = frame.line.paint(
                    bounds.origin,
                    frame.height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                return;
            }
            let baseline = f32::from(
                (frame.height - frame.line.ascent - frame.line.descent) / 2. + frame.line.ascent,
            );
            let center = f32::from(frame.height) / 2.;
            let color = window.text_style().color;
            for (g, p) in &frame.layers {
                if p.alpha <= 0.001 {
                    continue;
                }
                let origin = point(
                    bounds.left() + px(p.x + g.advance * (1. - p.scale) / 2.),
                    bounds.top() + px(center + (baseline + g.y - center) * p.scale + p.y),
                );
                if g.emoji {
                    if p.alpha >= 0.5 {
                        let _ = window.paint_emoji(origin, g.font, g.id, frame.size * p.scale);
                    }
                    continue;
                }
                let blur =
                    f32::from(frame.size) * BLUR * ((1. - p.scale) / (1. - SCALE)).clamp(0., 1.);
                if blur < 0.05 {
                    let _ = window.paint_glyph(
                        origin,
                        g.font,
                        g.id,
                        frame.size * p.scale,
                        color.opacity(p.alpha),
                    );
                } else {
                    // Normalized separable [1,2,1] Gaussian approximation. The
                    // center's alpha compensates source-over overlap, preserving ink.
                    for (y, wy) in [(-1., 0.25), (0., 0.5), (1., 0.25)] {
                        for (x, wx) in [(-1., 0.25), (0., 0.5), (1., 0.25)] {
                            let alpha = 1. - (1. - p.alpha.min(0.9999)).powf(wx * wy);
                            let _ = window.paint_glyph(
                                origin + point(px(x * blur), px(y * blur)),
                                g.font,
                                g.id,
                                frame.size * p.scale,
                                color.opacity(alpha),
                            );
                        }
                    }
                }
            }
        });
        if frame.active {
            window.request_animation_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Label {
        text: SharedString,
        renders: usize,
    }
    impl gpui::Render for Label {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            self.renders += 1;
            div()
                .text_size(px(13.))
                .line_height(px(20.))
                .child(RollingText::new("label", self.text.clone()))
        }
    }
    fn label(_: &mut Window, _: &mut gpui::Context<Label>) -> Label {
        Label {
            text: "Mixed".into(),
            renders: 0,
        }
    }

    #[gpui::test]
    fn rolling_text_requests_display_frames_and_stops_at_rest(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(false));
        let (view, cx) = cx.add_window_view(label);
        cx.run_until_parked();
        view.update(cx, |label, cx| {
            label.text = "Separate".into();
            cx.notify();
        });
        cx.run_until_parked();
        let before = view.read_with(cx, |label, _| label.renders);
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(8));
            cx.executor().advance_clock(Duration::from_millis(8));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
        }
        assert!(
            view.read_with(cx, |label, _| label.renders) >= before + 4,
            "moving labels must use requested display frames"
        );
        std::thread::sleep(Duration::from_millis(740));
        cx.executor().advance_clock(Duration::from_millis(740));
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        let settled = view.read_with(cx, |label, _| label.renders);
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |label, _| label.renders),
            settled,
            "no animation frames after settling"
        );
    }

    #[test]
    fn rolling_text_shared_runs_and_reference_curves() {
        for (from, to, ends) in [
            ("Mixed", "Mixed", (5, 0)),
            ("aa", "aaa", (2, 0)),
            ("feature/a", "fix/a", (1, 2)),
            ("éX界", "éYY界", (2, 3)),
            ("", "Both", (0, 0)),
        ] {
            assert_eq!(shared_ends(from, to), ends);
        }
        assert_eq!(glide(0.), 0.);
        assert_eq!(glide(1.), 1.);
        assert!((glide(0.5) - 0.96138).abs() < 0.0001);
        assert!((resize(0.5) - 0.68464).abs() < 0.0001);
        assert_eq!(DURATION * (1. + STAGGER), 0.715);
    }

    #[gpui::test]
    fn rolling_text_rapid_changes_preserve_visible_poses_and_width(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(label);
        cx.update(|window, _| {
            let font = gpui::font("Arial");
            let shape = |text: &str| {
                window.text_system().shape_line(
                    text.to_owned().into(),
                    px(13.),
                    &[TextRun {
                        len: text.len(),
                        font: font.clone(),
                        color: gpui::black(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
            };
            let now = Instant::now();
            let mut state = RollState::default();
            state.update(
                "Mixed".into(),
                &shape("Mixed"),
                font.clone(),
                px(13.),
                now,
                false,
            );
            assert!(!state.active(now));
            state.update(
                "Separate".into(),
                &shape("Separate"),
                font.clone(),
                px(13.),
                now,
                false,
            );
            let sample_time = now + Duration::from_millis(70);
            let width = state.width(sample_time);
            let visible: Vec<_> = state
                .layers
                .iter()
                .map(|l| (l.glyph.clone(), l.sample(sample_time)))
                .filter(|(_, p)| p.alpha > 0.001)
                .collect();
            state.update(
                "Both".into(),
                &shape("Both"),
                font.clone(),
                px(13.),
                sample_time,
                false,
            );
            assert_eq!(state.width(sample_time), width);
            for (glyph, pose) in visible {
                assert!(
                    state.layers.iter().any(|l| l.glyph.id == glyph.id
                        && l.glyph.index == glyph.index
                        && l.sample(sample_time) == pose),
                    "lost visible glyph {glyph:?}: {pose:?}"
                );
            }
            state.update(
                "Mixed".into(),
                &shape("Mixed"),
                font.clone(),
                px(13.),
                sample_time + Duration::from_millis(10),
                false,
            );
            let settled = now + Duration::from_secs(2);
            assert!(!state.active(settled));
            state.update(
                "Mixed".into(),
                &shape("Mixed"),
                font.clone(),
                px(13.),
                settled,
                false,
            );
            assert!(state.layers.iter().all(|l| l.current));
            assert_eq!(state.width(settled), f32::from(shape("Mixed").width).ceil());
            state.update(
                "Both".into(),
                &shape("Both"),
                font.clone(),
                px(13.),
                settled,
                false,
            );
            state.update("Both".into(), &shape("Both"), font, px(13.), settled, true);
            assert!(!state.active(settled));
            assert!(
                state
                    .layers
                    .iter()
                    .all(|l| l.sample(settled).alpha == 1. && l.sample(settled).scale == 1.)
            );
        });
    }

    #[gpui::test]
    fn rolling_text_shared_prefix_stays_still_and_tail_slides(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(label);
        cx.update(|window, _| {
            let font = gpui::font("Arial");
            let shape = |text: &str| {
                window.text_system().shape_line(
                    text.to_owned().into(),
                    px(13.),
                    &[TextRun {
                        len: text.len(),
                        font: font.clone(),
                        color: gpui::black(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
            };
            let now = Instant::now();
            let mut state = RollState::default();
            state.update(
                "aaXz".into(),
                &shape("aaXz"),
                font.clone(),
                px(13.),
                now,
                false,
            );
            state.update(
                "aaLongz".into(),
                &shape("aaLongz"),
                font.clone(),
                px(13.),
                now,
                false,
            );
            for layer in state
                .layers
                .iter()
                .filter(|l| l.current && l.glyph.index < 2)
            {
                assert_eq!(layer.from, layer.to);
            }
            let tail = state
                .layers
                .iter()
                .find(|l| l.current && l.glyph.index == 6)
                .unwrap();
            assert_eq!(tail.duration, RESIZE);
            assert_eq!(tail.from.alpha, 1.);
            assert_eq!(tail.to.alpha, 1.);
            assert_ne!(tail.from.x, tail.to.x);
            assert_eq!(tail.sample(now + Duration::from_millis(200)), tail.to);
            assert!(state.active(now + Duration::from_millis(7)));
            assert!(state.active(now + Duration::from_millis(550)));
            assert!(!state.active(now + Duration::from_millis(715)));
        });
    }
}
