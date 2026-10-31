//! Docked presentation panels, independent of capture and recording.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Panel {
    Preview,
    Audio,
    Display,
    Controls,
    Resources,
}
impl Panel {
    pub const ALL: [Self; 5] = [
        Self::Preview,
        Self::Audio,
        Self::Display,
        Self::Controls,
        Self::Resources,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Audio => "audio",
            Self::Display => "display",
            Self::Controls => "controls",
            Self::Resources => "resources",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Preview => "Preview",
            Self::Audio => "Audio",
            Self::Display => "Sources",
            Self::Controls => "Command Deck",
            Self::Resources => "Resources",
        }
    }
    pub fn minimum(self) -> (f32, f32) {
        match self {
            Self::Preview => (280., 180.),
            Self::Audio => (460., 156.),
            Self::Display => (208., 128.),
            Self::Controls => (264., 280.),
            Self::Resources => (264., 88.),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    Swap,
}
impl Edge {
    pub fn key(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Swap => "swap",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    Panel {
        panel: Panel,
    },
    Split {
        axis: Axis,
        ratio: u16,
        first: Box<Node>,
        second: Box<Node>,
    },
}
impl Node {
    fn panel(panel: Panel) -> Self {
        Self::Panel { panel }
    }
    fn split(axis: Axis, ratio: u16, first: Node, second: Node) -> Self {
        Self::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }
    fn stack(panels: &[Panel]) -> Self {
        if panels.len() == 1 {
            return Self::panel(panels[0]);
        }
        let middle = panels.len() / 2;
        Self::split(
            Axis::Vertical,
            500,
            Self::stack(&panels[..middle]),
            Self::stack(&panels[middle..]),
        )
    }
    fn leaves(&self, result: &mut Vec<Panel>) {
        match self {
            Self::Panel { panel } => result.push(*panel),
            Self::Split { first, second, .. } => {
                first.leaves(result);
                second.leaves(result);
            }
        }
    }
    fn minimum(&self, gap: f32) -> (f32, f32) {
        match self {
            Self::Panel { panel } => panel.minimum(),
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (a, b) = (first.minimum(gap), second.minimum(gap));
                match axis {
                    Axis::Horizontal => (a.0 + gap + b.0, a.1.max(b.1)),
                    Axis::Vertical => (a.0.max(b.0), a.1 + gap + b.1),
                }
            }
        }
    }
    fn remove(self, source: Panel) -> Option<Self> {
        match self {
            Self::Panel { panel } => (panel != source).then_some(Self::panel(panel)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(source), second.remove(source)) {
                (Some(a), Some(b)) => Some(Self::split(axis, ratio, a, b)),
                (a, b) => a.or(b),
            },
        }
    }
    fn insert(&mut self, source: Panel, target: Panel, edge: Edge, ratio: u16) {
        match self {
            Self::Panel { panel } if *panel == target => {
                let axis = if matches!(edge, Edge::Left | Edge::Right) {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let (a, b) = if matches!(edge, Edge::Left | Edge::Top) {
                    (source, target)
                } else {
                    (target, source)
                };
                *self = Self::split(axis, ratio, Self::panel(a), Self::panel(b));
            }
            Self::Split { first, second, .. } => {
                first.insert(source, target, edge, ratio);
                second.insert(source, target, edge, ratio);
            }
            _ => {}
        }
    }
    fn swap(&mut self, source: Panel, target: Panel) {
        match self {
            Self::Panel { panel } => {
                if *panel == source {
                    *panel = target
                } else if *panel == target {
                    *panel = source
                }
            }
            Self::Split { first, second, .. } => {
                first.swap(source, target);
                second.swap(source, target);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DockLayout {
    pub root: Node,
}
impl Default for DockLayout {
    fn default() -> Self {
        use Axis::*;
        use Panel::*;
        Self {
            root: Node::split(
                Vertical,
                630,
                Node::split(
                    Horizontal,
                    720,
                    Node::panel(Preview),
                    Node::split(Vertical, 800, Node::panel(Controls), Node::panel(Resources)),
                ),
                Node::split(Horizontal, 620, Node::panel(Audio), Node::panel(Display)),
            ),
        }
    }
}
impl<'de> Deserialize<'de> for DockLayout {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A broken UI layout must not invalidate otherwise valid recording preferences.
        let value = serde_json::Value::deserialize(deserializer)?;
        let candidate = value
            .get("root")
            .cloned()
            .and_then(|root| serde_json::from_value::<Node>(root).ok())
            .map(|root| {
                let mut layout = Self { root };
                // Preserve the old tree and ratios, adding only the missing
                // resource panel beneath Controls. Other invalid trees recover.
                if layout.valid_for(&Panel::ALL[..4]) {
                    layout
                        .root
                        .insert(Panel::Resources, Panel::Controls, Edge::Bottom, 800);
                }
                layout
            });
        Ok(candidate.filter(Self::valid).unwrap_or_default())
    }
}
impl DockLayout {
    pub fn valid(&self) -> bool {
        self.valid_for(&Panel::ALL)
    }
    fn valid_for(&self, expected: &[Panel]) -> bool {
        fn check(node: &Node, depth: usize, seen: &mut Vec<Panel>) -> bool {
            if depth >= Panel::ALL.len() {
                return false;
            }
            match node {
                Node::Panel { panel } => {
                    if seen.contains(panel) {
                        return false;
                    }
                    seen.push(*panel);
                    true
                }
                Node::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    (100..=900).contains(ratio)
                        && check(first, depth + 1, seen)
                        && check(second, depth + 1, seen)
                }
            }
        }
        let mut seen = Vec::with_capacity(Panel::ALL.len());
        check(&self.root, 0, &mut seen)
            && seen.len() == expected.len()
            && expected.iter().all(|panel| seen.contains(panel))
    }
    pub fn dock(&mut self, source: Panel, target: Panel, edge: Edge) -> bool {
        if source == target || !self.valid() {
            return false;
        }
        if edge == Edge::Swap {
            self.root.swap(source, target);
        } else {
            let mut root = self
                .root
                .clone()
                .remove(source)
                .expect("distinct dock panels");
            root.insert(source, target, edge, 500);
            self.root = root;
        }
        debug_assert!(self.valid());
        true
    }
    pub fn resize(&mut self, path: &[bool], ratio: u16) -> bool {
        let mut node = &mut self.root;
        for second in path {
            node = match node {
                Node::Split {
                    first, second: b, ..
                } => {
                    if *second {
                        b
                    } else {
                        first
                    }
                }
                _ => return false,
            };
        }
        if let Node::Split {
            ratio: previous, ..
        } = node
        {
            let next = ratio.clamp(100, 900);
            if *previous != next {
                *previous = next;
                return true;
            }
        }
        false
    }
    pub fn geometry(&self, width: f32, height: f32, gap: f32) -> Geometry {
        let width = width.max(1.);
        let gap = gap.clamp(6., 20.);
        let safe = if self.valid() {
            self.clone()
        } else {
            Self::default()
        };
        let minimum = safe.root.minimum(gap);
        let compact = minimum.0 > width || minimum.1 > height;
        let mut root = safe.root.clone();
        if compact {
            let mut order = vec![];
            safe.root.leaves(&mut order);
            let mut candidate = if width >= 600. {
                let middle = order.len().div_ceil(2);
                Node::split(
                    Axis::Horizontal,
                    500,
                    Node::stack(&order[..middle]),
                    Node::stack(&order[middle..]),
                )
            } else {
                Node::stack(&order)
            };
            if candidate.minimum(gap).0 > width {
                candidate = Node::stack(&order);
            }
            let candidate_minimum = candidate.minimum(gap);
            // A short viewport must not rebuild an already useful tree on every
            // frame, wiping its splitter ratios or even making it taller.
            if minimum.0 > width
                || (candidate_minimum.0 <= width && candidate_minimum.1 < minimum.1)
            {
                root = candidate;
            }
        }
        let canvas_height = height.max(root.minimum(gap).1);
        let mut geometry = Geometry {
            width,
            height: canvas_height,
            panes: vec![],
            splits: vec![],
            compact,
            layout: Self { root: root.clone() },
        };
        arrange(
            &root,
            Rect {
                x: 0.,
                y: 0.,
                width,
                height: canvas_height,
            },
            gap,
            &mut vec![],
            &mut geometry,
        );
        geometry
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Rect {
    pub fn fit(self, source: (u32, u32)) -> Self {
        let ratio = source.0.max(1) as f32 / source.1.max(1) as f32;
        let width = self.width.min(self.height * ratio).max(1.);
        let height = width / ratio;
        Self {
            x: self.x + (self.width - width) / 2.,
            y: self.y + (self.height - height) / 2.,
            width,
            height,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Split {
    pub path: Vec<bool>,
    pub axis: Axis,
    pub bounds: Rect,
    pub parent: Rect,
    min_first: f32,
    min_second: f32,
}
impl Split {
    pub fn cut(&self) -> f32 {
        match self.axis {
            Axis::Horizontal => self.bounds.x - self.parent.x,
            Axis::Vertical => self.bounds.y - self.parent.y,
        }
    }
    pub fn available(&self) -> f32 {
        match self.axis {
            Axis::Horizontal => self.parent.width - self.bounds.width,
            Axis::Vertical => self.parent.height - self.bounds.height,
        }
        .max(1.)
    }
    /// Store the visible cut, including child minimums, rather than an unreachable ratio.
    pub fn ratio_for_cut(&self, cut: f32) -> u16 {
        let available = self.available();
        let lower = self.min_first.min(available);
        let upper = (available - self.min_second).max(lower);
        (cut.clamp(lower, upper) / available * 1000.)
            .round()
            .clamp(100., 900.) as u16
    }
}
#[derive(Clone, Default, Debug)]
pub struct Geometry {
    pub width: f32,
    pub height: f32,
    pub panes: Vec<(Panel, Rect)>,
    pub splits: Vec<Split>,
    pub compact: bool,
    pub layout: DockLayout,
}
impl Geometry {
    pub fn pane(&self, panel: Panel) -> Rect {
        self.panes
            .iter()
            .find(|(p, _)| *p == panel)
            .map_or(Rect::default(), |(_, r)| *r)
    }
}
fn arrange(node: &Node, rect: Rect, gap: f32, path: &mut Vec<bool>, out: &mut Geometry) {
    match node {
        Node::Panel { panel } => out.panes.push((*panel, rect)),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (a, b) = (first.minimum(gap), second.minimum(gap));
            let horizontal = *axis == Axis::Horizontal;
            let length = if horizontal { rect.width } else { rect.height };
            let available = (length - gap).max(1.);
            let (min_a, min_b) = if horizontal { (a.0, b.0) } else { (a.1, b.1) };
            let cut = (available * *ratio as f32 / 1000.).clamp(
                min_a.min(available),
                (available - min_b).max(min_a.min(available)),
            );
            let (one, divider, two) = if horizontal {
                (
                    Rect { width: cut, ..rect },
                    Rect {
                        x: rect.x + cut,
                        width: gap,
                        ..rect
                    },
                    Rect {
                        x: rect.x + cut + gap,
                        width: (available - cut).max(1.),
                        ..rect
                    },
                )
            } else {
                (
                    Rect {
                        height: cut,
                        ..rect
                    },
                    Rect {
                        y: rect.y + cut,
                        height: gap,
                        ..rect
                    },
                    Rect {
                        y: rect.y + cut + gap,
                        height: (available - cut).max(1.),
                        ..rect
                    },
                )
            };
            out.splits.push(Split {
                path: path.clone(),
                axis: *axis,
                bounds: divider,
                parent: rect,
                min_first: min_a,
                min_second: min_b,
            });
            path.push(false);
            arrange(first, one, gap, path, out);
            path.pop();
            path.push(true);
            arrange(second, two, gap, path, out);
            path.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_dock_move_preserves_all_panels_and_round_trips() {
        for source in Panel::ALL {
            for target in Panel::ALL {
                for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom, Edge::Swap] {
                    let mut layout = DockLayout::default();
                    assert_eq!(layout.dock(source, target, edge), source != target);
                    assert!(layout.valid());
                    assert_eq!(
                        serde_json::from_str::<DockLayout>(
                            &serde_json::to_string(&layout).unwrap()
                        )
                        .unwrap(),
                        layout
                    );
                }
            }
        }
    }
    #[test]
    fn old_four_panel_layouts_add_resources_without_resetting_the_tree() {
        let legacy = serde_json::json!({"root":{
            "kind":"split","axis":"vertical","ratio":630,
            "first":{"kind":"split","axis":"horizontal","ratio":634,
                "first":{"kind":"panel","panel":"preview"},
                "second":{"kind":"panel","panel":"controls"}},
            "second":{"kind":"split","axis":"horizontal","ratio":586,
                "first":{"kind":"panel","panel":"audio"},
                "second":{"kind":"panel","panel":"display"}}
        }});
        let old_root: Node = serde_json::from_value(legacy["root"].clone()).unwrap();
        let migrated: DockLayout = serde_json::from_value(legacy).unwrap();
        assert!(migrated.valid());
        assert_eq!(
            migrated.root.clone().remove(Panel::Resources).unwrap(),
            old_root
        );
        let geometry = migrated.geometry(1600., 1000., 10.);
        assert!(geometry.pane(Panel::Resources).y > geometry.pane(Panel::Controls).y);
        let serialized = serde_json::to_string(&migrated).unwrap();
        assert_eq!(
            serde_json::from_str::<DockLayout>(&serialized).unwrap(),
            migrated
        );
    }

    #[test]
    fn corrupt_or_old_layouts_recover_without_losing_other_preferences() {
        let mut prefs = serde_json::to_value(super::super::Preferences::default()).unwrap();
        prefs["countdown"] = 3.into();
        for bad in [
            serde_json::json!({"root":{"kind":"panel","panel":"audio"}}),
            serde_json::json!({"root":null}),
        ] {
            prefs["workspace"] = bad;
            let loaded: super::super::Preferences = serde_json::from_value(prefs.clone()).unwrap();
            assert_eq!(loaded.workspace, DockLayout::default());
            assert_eq!(loaded.countdown, 3);
        }
        prefs.as_object_mut().unwrap().remove("workspace");
        assert_eq!(
            serde_json::from_value::<super::super::Preferences>(prefs)
                .unwrap()
                .workspace,
            DockLayout::default()
        );
    }
    #[test]
    fn responsive_bounds_and_source_aspect_survive_all_dock_moves() {
        for source in Panel::ALL {
            for target in Panel::ALL {
                for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom, Edge::Swap] {
                    let mut layout = DockLayout::default();
                    layout.dock(source, target, edge);
                    for (w, h) in [
                        (776., 520.),
                        (1256., 820.),
                        (1896., 970.),
                        (3816., 520.),
                        (476., 520.),
                    ] {
                        let before = layout.clone();
                        let geometry = layout.geometry(w, h, 10.);
                        assert_eq!(geometry.panes.len(), Panel::ALL.len());
                        assert!(
                            geometry.layout.valid(),
                            "compact layout retains every panel"
                        );
                        for (_, b) in &geometry.panes {
                            assert!(
                                b.x >= 0.
                                    && b.y >= 0.
                                    && b.x + b.width <= w + 0.1
                                    && b.y + b.height <= geometry.height + 0.1,
                                "{b:?}"
                            );
                        }
                        for source in [(1920, 1080), (2560, 1600), (1080, 1920)] {
                            let b = geometry.pane(Panel::Preview).fit(source);
                            assert!(
                                (b.width / b.height - source.0 as f32 / source.1 as f32).abs()
                                    < 0.001
                            );
                        }
                        assert_eq!(
                            layout, before,
                            "Responsive fallback must not replace the saved layout"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn short_viewports_preserve_useful_trees_and_explicit_split_ratios() {
        let saved = DockLayout::default();
        let short = saved.geometry(776., 420., 10.);
        assert_eq!(
            short.layout, saved,
            "A taller fallback must not replace the saved tree"
        );

        let tall = DockLayout {
            root: Node::stack(&Panel::ALL),
        };
        let compact = tall.geometry(1200., 420., 10.);
        assert_ne!(compact.layout, tall, "A shorter fallback remains available");
        let mut resized = compact.layout;
        assert!(resized.resize(&[], 550));
        assert_eq!(
            resized.geometry(1200., 420., 10.).layout,
            resized,
            "Scrolling must retain the explicitly resized compact tree"
        );
    }
    #[test]
    fn a_split_at_its_minimum_responds_when_the_pointer_reverses() {
        let mut layout = DockLayout::default();
        let geometry = layout.geometry(1600., 1000., 10.);
        let split = geometry
            .splits
            .iter()
            .find(|s| s.axis == Axis::Horizontal)
            .unwrap();
        let path = split.path.clone();
        let limit = split.ratio_for_cut(-1000.);
        assert!(layout.resize(&path, limit));
        let geometry = layout.geometry(1600., 1000., 10.);
        let split = geometry.splits.iter().find(|s| s.path == path).unwrap();
        assert_eq!(split.ratio_for_cut(split.cut() - 1000.), limit);
        assert!(split.ratio_for_cut(split.cut() + 20.) > limit);
    }
    #[test]
    fn split_resize_is_bounded_and_invalid_paths_are_ignored() {
        let mut layout = DockLayout::default();
        assert!(layout.resize(&[], 0));
        assert!(!layout.resize(&[false, false, false], 500));
        assert!(layout.valid());
        let bounds = layout.geometry(776., 520., 10.);
        for (panel, rect) in bounds.panes {
            let min = panel.minimum();
            assert!(rect.width >= min.0 && rect.height >= min.1);
        }
    }
}
