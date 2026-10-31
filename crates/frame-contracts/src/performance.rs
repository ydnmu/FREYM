//! Resource policies never alter the recording request's quality or frame geometry.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PerformanceMode {
    Maximum,
    #[default]
    Balanced,
    Efficiency,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CpuPriority {
    #[default]
    Normal,
    High,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformancePolicy {
    pub mode: PerformanceMode,
    pub cpu: CpuPriority,
    pub adaptive: bool,
}
impl PerformancePolicy {
    pub fn preview_hz(self) -> u32 {
        match self.mode {
            PerformanceMode::Maximum => 60,
            PerformanceMode::Balanced => 30,
            PerformanceMode::Efficiency => 15,
        }
    }
}
