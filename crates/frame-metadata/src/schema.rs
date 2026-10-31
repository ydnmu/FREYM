use serde::{Deserialize, Serialize};
use std::io;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_RECORD_BYTES: usize = 4096;
pub const MAX_SIDECAR_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Native,
    Sampled,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub cursor: Support,
    pub buttons: Support,
    pub scroll: Support,
    pub geometry: Support,
    /// Only whether the selected source has focus, never another window's identity.
    pub source_focus: Support,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            cursor: Support::Unsupported,
            buttons: Support::Unsupported,
            scroll: Support::Unsupported,
            geometry: Support::Unsupported,
            source_focus: Support::Unsupported,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub version: u32,
    /// Same MasterClock epoch as capture/audio. No wall-clock or account identifier.
    pub origin_ns: u64,
    /// Whether capture already rendered the cursor into video. Unknown is explicit
    /// on portable adapters until they expose this native capture fact.
    #[serde(default)]
    pub cursor_baked: Option<bool>,
    pub capabilities: Capabilities,
}

/// Physical desktop pixels. Cursor coordinates are relative to this rectangle's
/// top-left; output pixels are obtained by scaling to the segment's track size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Geometry {
    pub fn relative(&self, x: i32, y: i32) -> Option<(u32, u32)> {
        let x = i64::from(x) - i64::from(self.x);
        let y = i64::from(y) - i64::from(self.y);
        (x >= 0 && y >= 0 && x < i64::from(self.width) && y < i64::from(self.height))
            .then_some((x as u32, y as u32))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
    Extra1,
    Extra2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Started,
    Paused,
    Resumed,
    Stopping,
    Completed,
    Interrupted,
}

/// Additive, versioned typed events leave room for future editor effects without
/// permitting arbitrary sensitive text in the capture API. Unknown event kinds
/// require a newer reader, rather than silently discarding timeline information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventKind {
    Cursor {
        x: u32,
        y: u32,
    },
    CursorLeft,
    Button {
        x: u32,
        y: u32,
        button: Button,
        pressed: bool,
    },
    /// Native signed wheel units, 120 per detent on Windows; no fabricated pixels.
    Scroll {
        x: u32,
        y: u32,
        horizontal: bool,
        delta: i32,
    },
    Geometry {
        bounds: Geometry,
    },
    SourceUnavailable,
    Focus {
        active: bool,
    },
    Lifecycle {
        state: Lifecycle,
    },
    Segment {
        origin_ns: u64,
    },
    /// Cumulative ingress loss (full queue or timeline contention).
    Loss {
        dropped: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    pub sequence: u64,
    pub raw_session_ns: u64,
    /// PauseTimeline.map(raw), exactly the timebase used by admitted A/V.
    /// MKV PTS = session_ns - segment.origin_ns, rounded to TimestampScale.
    pub session_ns: u64,
    pub event: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "record",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Record {
    Header(Header),
    Event(Event),
    Checkpoint {
        events: u64,
        dropped: u64,
    },
    End {
        completed: bool,
        events: u64,
        dropped: u64,
    },
}

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Default)]
pub(crate) struct Validator {
    pub header: Option<Header>,
    pub events: u64,
    pub ended: bool,
    last: Option<(u64, u64, u64)>,
}
impl Validator {
    pub fn accept(&mut self, record: &Record) -> io::Result<()> {
        if self.ended {
            return Err(invalid("Records after metadata end."));
        }
        match record {
            Record::Header(header) if self.header.is_none() && header.version == SCHEMA_VERSION => {
                self.header = Some(header.clone());
            }
            Record::Event(event) if self.header.is_some() => {
                if event.version != SCHEMA_VERSION
                    || event.session_ns < self.header.as_ref().unwrap().origin_ns
                    || event.session_ns > event.raw_session_ns
                    || self.last.is_some_and(|(seq, raw, media)| {
                        event.sequence <= seq
                            || event.raw_session_ns < raw
                            || event.session_ns < media
                    })
                {
                    return Err(invalid("Invalid metadata version, sequence or timestamp."));
                }
                if let EventKind::Geometry { bounds } = event.event
                    && (bounds.width == 0 || bounds.height == 0)
                {
                    return Err(invalid("Empty source geometry."));
                }
                self.last = Some((event.sequence, event.raw_session_ns, event.session_ns));
                self.events += 1;
            }
            Record::Checkpoint { events, .. }
                if self.header.is_some() && *events == self.events => {}
            Record::End { events, .. } if self.header.is_some() && *events == self.events => {
                self.ended = true
            }
            _ => return Err(invalid("Invalid metadata record order or schema.")),
        }
        Ok(())
    }
}
