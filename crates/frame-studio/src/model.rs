use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_EVENTS: usize = 1_000_000;
pub const MAX_DOCUMENT_BYTES: u64 = 96 * 1024 * 1024;
pub type StudioResult<T> = Result<T, String>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
impl Point {
    pub fn valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
    }
    fn mix(self, other: Self, weight: f64) -> Self {
        Self {
            x: self.x + (other.x - self.x) * weight,
            y: self.y + (other.y - self.y) * weight,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum InteractionKind {
    Move,
    Click,
    Focus,
    Scroll,
    Marker,
    Leave,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Interaction {
    pub time_ns: u64,
    pub position: Point,
    pub kind: InteractionKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InteractionMetadata {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    /// Cursor pixels already in the source cannot be removed by smoothing.
    pub cursor_baked: bool,
    pub origin_ns: u64,
    pub sampling_interval_ns: u64,
    pub truncated: bool,
    pub events: Vec<Interaction>,
}
impl InteractionMetadata {
    pub fn validate(&self) -> StudioResult<()> {
        if self.version != 1
            || self.width == 0
            || self.height == 0
            || self.events.len() > MAX_EVENTS
        {
            return Err("Unsupported or oversized interaction metadata.".into());
        }
        let mut last = 0;
        for e in &self.events {
            if e.time_ns < last || !e.position.valid() {
                return Err("Invalid metadata coordinates or event order.".into());
            }
            last = e.time_ns;
        }
        Ok(())
    }
}
pub fn sidecar_path(source: &Path, suffix: &str) -> PathBuf {
    let mut name = source.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceIdentity {
    pub bytes: u64,
    pub modified_ns: u128,
}
impl SourceIdentity {
    pub fn read(source: &Path) -> StudioResult<Self> {
        let info = fs::metadata(source).map_err(|e| e.to_string())?;
        Ok(Self {
            bytes: info.len(),
            modified_ns: info
                .modified()
                .map_err(|e| e.to_string())?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos(),
        })
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Easing {
    Smooth,
    Linear,
}
fn ease(t: f64, easing: Easing) -> f64 {
    let t = t.clamp(0.0, 1.0);
    match easing {
        Easing::Linear => t,
        Easing::Smooth => t * t * t * (10.0 + t * (-15.0 + 6.0 * t)),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZoomSegment {
    pub enabled: bool,
    pub start_ns: u64,
    pub end_ns: u64,
    pub ramp_ns: u64,
    pub scale: f64,
    pub focus: Point,
    pub easing: Easing,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CursorEffects {
    pub enabled: bool,
    pub smoothing_ms: u32,
    pub emphasis: bool,
    pub size: f64,
}
/// Conversion is an edit decision. Preserve refuses HDR/SDR or PQ/HLG mixtures
/// until the user chooses a common output transfer; it never guesses a tone map.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputColour {
    #[default]
    Preserve,
    HdrPq,
    HdrHlg,
    Sdr,
}
impl Default for CursorEffects {
    fn default() -> Self {
        Self {
            enabled: false,
            smoothing_ms: 90,
            emphasis: true,
            size: 1.0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Timeline {
    pub version: u32,
    pub source: PathBuf,
    pub identity: SourceIdentity,
    pub duration_ns: u64,
    /// Snapshot events with the edit decisions, so deleting a sidecar does not silently change an edit.
    pub metadata: Option<InteractionMetadata>,
    pub zooms: Vec<ZoomSegment>,
    pub cursor: CursorEffects,
    /// Version 2: immutable sources and ordered, source-local edit ranges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceSegment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<TimelineClip>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub output_colour: OutputColour,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceSegment {
    pub path: PathBuf,
    pub identity: SourceIdentity,
    pub origin_ns: u64,
    pub duration_ns: u64,
    pub metadata: Option<InteractionMetadata>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineClip {
    pub source_index: usize,
    pub in_ns: u64,
    pub out_ns: u64,
}
impl TimelineClip {
    pub fn duration_ns(&self) -> u64 {
        self.out_ns - self.in_ns
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}
impl Timeline {
    pub fn validate(&self) -> StudioResult<()> {
        if !(1..=2).contains(&self.version) || self.duration_ns == 0 || self.zooms.len() > 4096 {
            return Err("Unsupported or oversized edit timeline.".into());
        }
        if self.version == 2 {
            if self.sources.is_empty()
                || self.sources.len() > 256
                || self.clips.is_empty()
                || self.clips.len() > 4096
            {
                return Err("Invalid segment count in edit timeline.".into());
            }
            let mut duration = 0u64;
            for clip in &self.clips {
                let source = self
                    .sources
                    .get(clip.source_index)
                    .ok_or("Edit refers to a missing source segment.")?;
                if clip.in_ns >= clip.out_ns || clip.out_ns > source.duration_ns {
                    return Err("Invalid source trim range.".into());
                }
                duration = duration
                    .checked_add(clip.duration_ns())
                    .ok_or("Edit duration overflow.")?;
            }
            if duration != self.duration_ns {
                return Err("Edit duration does not match its clip ranges.".into());
            }
            for source in &self.sources {
                if let Some(m) = &source.metadata {
                    m.validate()?;
                }
            }
        } else if !self.sources.is_empty() || !self.clips.is_empty() {
            return Err("Version 1 projects cannot contain segment edits.".into());
        }
        if let Some(metadata) = &self.metadata {
            metadata.validate()?;
        }
        let mut end = 0;
        for z in &self.zooms {
            if z.start_ns < end
                || z.end_ns <= z.start_ns
                || z.end_ns > self.duration_ns
                || z.ramp_ns == 0
                || z.ramp_ns > (z.end_ns - z.start_ns) / 2
                || !z.focus.valid()
                || !z.scale.is_finite()
                || !(1.0..=8.0).contains(&z.scale)
            {
                return Err("Zooms must be ordered, non-overlapping, within the clip and have valid scale/ramp/focus.".into());
            }
            end = z.end_ns;
        }
        if !self.cursor.size.is_finite()
            || !(0.5..=4.0).contains(&self.cursor.size)
            || self.cursor.smoothing_ms > 1000
        {
            return Err("Invalid cursor effects.".into());
        }
        if self.cursor.enabled
            && self.cursor.smoothing_ms > 0
            && (self.metadata.as_ref().is_some_and(|m| m.cursor_baked)
                || self
                    .sources
                    .iter()
                    .any(|s| s.metadata.as_ref().is_some_and(|m| m.cursor_baked)))
        {
            return Err(
                "Cursor smoothing requires a recording captured without baked cursor pixels."
                    .into(),
            );
        }
        Ok(())
    }
    pub fn verify_source(&self) -> StudioResult<()> {
        self.validate()?;
        if SourceIdentity::read(&self.source)? != self.identity {
            return Err("The source media has changed since this edit was saved.".into());
        }
        for source in &self.sources {
            if SourceIdentity::read(&source.path)? != source.identity {
                return Err(format!("Source segment changed: {}", source.path.display()));
            }
        }
        Ok(())
    }
    pub fn upgrade(&mut self) {
        if self.version == 1 {
            self.sources = vec![SourceSegment {
                path: self.source.clone(),
                identity: self.identity.clone(),
                origin_ns: self.metadata.as_ref().map_or(0, |m| m.origin_ns),
                duration_ns: self.duration_ns,
                metadata: self.metadata.clone(),
            }];
            self.clips = vec![TimelineClip {
                source_index: 0,
                in_ns: 0,
                out_ns: self.duration_ns,
            }];
            self.version = 2;
        }
    }
    pub fn locate(&self, time_ns: u64) -> Option<(usize, u64, u64)> {
        let mut start = 0;
        for (index, clip) in self.clips.iter().enumerate() {
            if time_ns < start + clip.duration_ns() {
                return Some((index, clip.in_ns + time_ns.saturating_sub(start), start));
            }
            start += clip.duration_ns();
        }
        None
    }
    pub fn append(&mut self, mut other: Timeline) -> StudioResult<()> {
        self.verify_source()?;
        other.verify_source()?;
        self.upgrade();
        other.upgrade();
        if self.sources.len() + other.sources.len() > 256
            || self.clips.len() + other.clips.len() > 4096
            || self.zooms.len() + other.zooms.len() > 4096
        {
            return Err(
                "Appending exceeds the project segment/effect limit. No source was discarded."
                    .into(),
            );
        }
        let duration = self
            .duration_ns
            .checked_add(other.duration_ns)
            .ok_or("Timeline duration overflow.")?;
        let base = self.sources.len();
        for clip in &mut other.clips {
            clip.source_index += base;
        }
        for zoom in &mut other.zooms {
            zoom.start_ns += self.duration_ns;
            zoom.end_ns += self.duration_ns;
        }
        self.sources.extend(other.sources);
        self.clips.extend(other.clips);
        self.zooms.extend(other.zooms);
        self.warnings.extend(other.warnings);
        self.duration_ns = duration;
        if self
            .sources
            .iter()
            .any(|s| s.metadata.as_ref().is_some_and(|m| m.cursor_baked))
        {
            self.cursor.smoothing_ms = 0;
        }
        self.rebuild_metadata();
        self.validate()
    }
    /// Split is lossless: adjacent ranges use the exact same boundary.
    pub fn split(&mut self, time_ns: u64) -> StudioResult<()> {
        self.upgrade();
        if self.clips.len() >= 4096 {
            return Err("Clip limit reached.".into());
        }
        let (index, at, _) = self
            .locate(time_ns)
            .ok_or("Split is outside the timeline.")?;
        let clip = self.clips[index].clone();
        if at <= clip.in_ns || at >= clip.out_ns {
            return Err("Choose a frame inside the clip to split.".into());
        }
        self.clips[index].out_ns = at;
        self.clips
            .insert(index + 1, TimelineClip { in_ns: at, ..clip });
        self.validate()
    }
    pub fn trim(&mut self, index: usize, in_ns: u64, out_ns: u64) -> StudioResult<()> {
        self.upgrade();
        let clip = self.clips.get(index).ok_or("No selected clip.")?;
        if in_ns < clip.in_ns || out_ns > clip.out_ns || in_ns >= out_ns {
            return Err("Trim must keep a non-empty range inside the selected clip.".into());
        }
        let mut ranges: Vec<(usize, u64, u64)> = self
            .clips
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.in_ns, c.out_ns))
            .collect();
        ranges[index] = (index, in_ns, out_ns);
        self.reassemble(&ranges)
    }
    pub fn move_clip(&mut self, index: usize, destination: usize) -> StudioResult<()> {
        self.upgrade();
        if index >= self.clips.len() || destination >= self.clips.len() {
            return Err("No clip at this position.".into());
        }
        let mut ranges: Vec<_> = self
            .clips
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.in_ns, c.out_ns))
            .collect();
        let range = ranges.remove(index);
        ranges.insert(destination, range);
        self.reassemble(&ranges)
    }
    fn reassemble(&mut self, ranges: &[(usize, u64, u64)]) -> StudioResult<()> {
        let old = self.clone();
        let mut starts = Vec::new();
        let mut start = 0;
        for clip in &old.clips {
            starts.push(start);
            start += clip.duration_ns();
        }
        self.clips.clear();
        self.zooms.clear();
        let mut output = 0;
        for &(index, from, to) in ranges {
            let clip = &old.clips[index];
            let old_start = starts[index] + from - clip.in_ns;
            let old_end = old_start + to - from;
            for zoom in &old.zooms {
                let a = zoom.start_ns.max(old_start);
                let b = zoom.end_ns.min(old_end);
                if b.saturating_sub(a) < 2 {
                    continue;
                }
                let mut zoom = zoom.clone();
                zoom.start_ns = output + a - old_start;
                zoom.end_ns = output + b - old_start;
                zoom.ramp_ns = zoom.ramp_ns.min((b - a) / 2);
                self.zooms.push(zoom);
            }
            self.clips.push(TimelineClip {
                source_index: clip.source_index,
                in_ns: from,
                out_ns: to,
            });
            output += to - from;
        }
        self.duration_ns = output;
        self.rebuild_metadata();
        self.validate()
    }
    pub fn rebuild_metadata(&mut self) {
        let Some(first) = self.sources.iter().find_map(|s| s.metadata.as_ref()) else {
            self.metadata = None;
            return;
        };
        let mut metadata = first.clone();
        metadata.events.clear();
        metadata.origin_ns = 0;
        metadata.cursor_baked = self
            .sources
            .iter()
            .any(|s| s.metadata.as_ref().is_some_and(|m| m.cursor_baked));
        let mut start = 0;
        for clip in &self.clips {
            // Reset smoothing/visibility at every cut, including metadata-free clips.
            metadata.events.push(Interaction {
                time_ns: start,
                position: Point { x: 0.5, y: 0.5 },
                kind: InteractionKind::Leave,
            });
            if let Some(m) = &self.sources[clip.source_index].metadata {
                metadata.truncated |= m.truncated;
                if let Some(e) = m.events.iter().rev().find(|e| e.time_ns < clip.in_ns) {
                    metadata.events.push(Interaction {
                        time_ns: start,
                        position: e.position,
                        kind: if e.kind == InteractionKind::Leave {
                            InteractionKind::Leave
                        } else {
                            InteractionKind::Move
                        },
                    });
                }
                for e in m
                    .events
                    .iter()
                    .filter(|e| e.time_ns >= clip.in_ns && e.time_ns < clip.out_ns)
                {
                    let mut e = e.clone();
                    e.time_ns = start + e.time_ns - clip.in_ns;
                    metadata.events.push(e);
                    if metadata.events.len() >= MAX_EVENTS {
                        metadata.truncated = true;
                        break;
                    }
                }
            }
            start += clip.duration_ns();
        }
        metadata.events.truncate(MAX_EVENTS);
        self.metadata = Some(metadata);
    }
    pub fn viewport(&self, time_ns: u64) -> Viewport {
        let mut view = Viewport {
            left: 0.0,
            top: 0.0,
            width: 1.0,
            height: 1.0,
        };
        if let Some(z) = self
            .zooms
            .iter()
            .find(|z| z.enabled && time_ns >= z.start_ns && time_ns < z.end_ns)
        {
            let weight = ease(
                ((time_ns - z.start_ns).min(z.end_ns - time_ns) as f64 / z.ramp_ns as f64).min(1.0),
                z.easing,
            );
            let size = 1.0 / (1.0 + (z.scale - 1.0) * weight);
            let target = Point {
                x: (z.focus.x - 0.5 / z.scale).clamp(0.0, 1.0 - 1.0 / z.scale),
                y: (z.focus.y - 0.5 / z.scale).clamp(0.0, 1.0 - 1.0 / z.scale),
            };
            // Interpolate the focus as well as the scale; edges stay inside source geometry.
            view = Viewport {
                left: (target.x * weight).min(1.0 - size),
                top: (target.y * weight).min(1.0 - size),
                width: size,
                height: size,
            };
        }
        view
    }
    pub fn cursor_at(&self, time_ns: u64) -> Option<(Point, bool)> {
        let m = self.metadata.as_ref()?;
        let until = m.events.partition_point(|e| e.time_ns <= time_ns);
        let last = m.events.get(until.checked_sub(1)?)?;
        if last.kind == InteractionKind::Leave {
            return None;
        }
        let mut position = last.position;
        if self.cursor.smoothing_ms > 0 && !m.cursor_baked {
            let horizon = u64::from(self.cursor.smoothing_ms) * 1_000_000;
            let from = m.events[..until]
                .partition_point(|e| e.time_ns < time_ns.saturating_sub(horizon * 6));
            let events = &m.events[from..until];
            if let Some(first) = events.first() {
                position = first.position;
                let mut previous = first.time_ns;
                for event in &events[1..] {
                    if event.kind == InteractionKind::Leave {
                        position = event.position;
                    } else {
                        position = position.mix(
                            event.position,
                            1.0 - (-((event.time_ns - previous) as f64) / horizon as f64).exp(),
                        );
                    }
                    previous = event.time_ns;
                }
                position = position.mix(
                    last.position,
                    1.0 - (-((time_ns - last.time_ns) as f64) / horizon as f64).exp(),
                );
            }
        }
        let clicked = m.events[..until]
            .iter()
            .rev()
            .take_while(|e| time_ns - e.time_ns <= 350_000_000)
            .any(|e| e.kind == InteractionKind::Click);
        Some((position, clicked))
    }
    pub fn suggest_zooms(&mut self, scale: f64) {
        let Some(m) = &self.metadata else {
            return;
        };
        let mut suggestions: Vec<ZoomSegment> = Vec::new();
        // Click/focus events and a cursor that remains in a small screen-relative area.
        let mut dwell: Option<&Interaction> = None;
        for (index, e) in m.events.iter().enumerate() {
            let stable_until = m
                .events
                .get(index + 1)
                .map_or(self.duration_ns, |next| next.time_ns);
            let dwell_trigger = e.kind == InteractionKind::Move
                && stable_until.saturating_sub(e.time_ns) >= 900_000_000;
            let focus = match e.kind {
                InteractionKind::Click | InteractionKind::Focus => true,
                InteractionKind::Move if dwell_trigger => true,
                InteractionKind::Move => {
                    let anchor = dwell.get_or_insert(e);
                    let distance = ((e.position.x - anchor.position.x) * m.width as f64)
                        .hypot((e.position.y - anchor.position.y) * m.height as f64);
                    if distance > m.width.min(m.height) as f64 * 0.035 {
                        dwell = Some(e);
                        false
                    } else if e.time_ns - anchor.time_ns >= 900_000_000 {
                        dwell = Some(e);
                        true
                    } else {
                        false
                    }
                }
                InteractionKind::Leave => {
                    dwell = None;
                    if let Some(last) = suggestions.last_mut()
                        && e.time_ns > last.start_ns
                    {
                        last.end_ns = last.end_ns.min(e.time_ns);
                        last.ramp_ns = last.ramp_ns.min((last.end_ns - last.start_ns) / 2);
                    }
                    false
                }
                _ => false,
            };
            if !focus || e.time_ns >= self.duration_ns {
                continue;
            }
            let focus_time = if dwell_trigger {
                e.time_ns + 900_000_000
            } else {
                e.time_ns
            };
            let start = focus_time.saturating_sub(350_000_000);
            let end = focus_time
                .saturating_add(1_600_000_000)
                .min(self.duration_ns);
            if let Some(last) = suggestions.last_mut()
                && start <= last.end_ns
            {
                if (last.focus.x - e.position.x).hypot(last.focus.y - e.position.y) < 0.22 {
                    last.end_ns = end;
                }
                continue;
            }
            if end - start < 200_000_000 {
                continue;
            }
            suggestions.push(ZoomSegment {
                enabled: true,
                start_ns: start,
                end_ns: end,
                ramp_ns: 350_000_000.min((end - start) / 2),
                scale: scale.clamp(1.0, 8.0),
                focus: e.position,
                easing: Easing::Smooth,
            });
            if suggestions.len() == 4096 {
                break;
            }
        }
        suggestions.retain(|z| z.end_ns.saturating_sub(z.start_ns) >= 200_000_000 && z.ramp_ns > 0);
        self.zooms = suggestions;
    }
    pub fn save(&self, path: &Path) -> StudioResult<()> {
        self.verify_source()?;
        write_json(path, self)
    }
    pub fn load(path: &Path) -> StudioResult<Self> {
        let value: Self = read_json(path)?;
        value.verify_source()?;
        Ok(value)
    }
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> StudioResult<T> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err("Studio document exceeds the local size limit.".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> StudioResult<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err("Studio document exceeds the local size limit.".into());
    }
    let temp = sidecar_path(
        path,
        &format!(
            ".{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ),
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| e.to_string())
}
