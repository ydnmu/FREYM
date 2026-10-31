//! Optional local interaction timeline. No video surfaces, keyboard, text, window
//! titles, process identities, clipboard, or network access enter this crate.
//! Native observers use try-lock/try-send; only the writer thread touches disk.
mod rolling;
mod schema;
mod store;
mod writer;

pub use rolling::{ReplayMetadataSnapshot, RollingMetadata};
pub use schema::*;
pub use store::{ImportSummary, export_jsonl, import_jsonl, read_sidecar, recover_sidecar};
pub use writer::{EventIngress, MetadataOptions, MetadataRecorder};
