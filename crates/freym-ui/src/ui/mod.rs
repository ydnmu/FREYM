//! Freym's public presentation API. Components never call media services.
#[cfg(feature = "ui-preview")]
pub(crate) mod component_lab;
pub mod components;
/// Isolated specimens derived from the browser reference, before product adoption.
#[cfg(feature = "ui-preview")]
pub mod lab_reference;
pub mod overlays;
pub mod primitives;
#[cfg(test)]
mod tests;
pub mod theme;
