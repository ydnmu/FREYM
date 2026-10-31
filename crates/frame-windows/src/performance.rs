//! Scoped media-worker policies. No process priority, affinity or GPU scheduler edits.
use frame_contracts::{CpuPriority, PerformanceMode, PerformancePolicy};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use windows::Win32::{Foundation::HANDLE, System::Threading::*};
static POLICY: AtomicU32 = AtomicU32::new(1);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static FAILURE: Mutex<Option<String>> = Mutex::new(None);
pub fn configure(policy: PerformancePolicy) {
    POLICY.store(
        match policy.mode {
            PerformanceMode::Maximum => 0,
            PerformanceMode::Balanced => 1,
            PerformanceMode::Efficiency => 2,
        } | if policy.cpu == CpuPriority::High {
            4
        } else {
            0
        } | if policy.adaptive { 8 } else { 0 },
        Ordering::Release,
    );
    *FAILURE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
pub fn policy() -> PerformancePolicy {
    let p = POLICY.load(Ordering::Acquire);
    PerformancePolicy {
        mode: match p & 3 {
            0 => PerformanceMode::Maximum,
            2 => PerformanceMode::Efficiency,
            _ => PerformanceMode::Balanced,
        },
        cpu: if p & 4 != 0 {
            CpuPriority::High
        } else {
            CpuPriority::Normal
        },
        adaptive: p & 8 != 0,
    }
}
pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Release);
}
pub fn failure() -> Option<String> {
    FAILURE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
pub(crate) fn unavailable(error: String) {
    *FAILURE.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
}
#[derive(Default)]
pub(crate) struct ThreadPolicy {
    applied: Option<(bool, PerformancePolicy)>,
    power: bool,
}
impl ThreadPolicy {
    pub(crate) fn refresh(&mut self, handle: HANDLE) {
        let active = ACTIVE.load(Ordering::Acquire);
        let p = policy();
        if self.applied == Some((active, p)) {
            return;
        }
        self.applied = Some((active, p));
        // SAFETY: MMCSS handle belongs to this calling thread's CaptureScheduling guard.
        if let Err(e) = unsafe {
            AvSetMmThreadPriority(
                handle,
                if active && p.cpu == CpuPriority::High {
                    AVRT_PRIORITY_HIGH
                } else {
                    AVRT_PRIORITY_NORMAL
                },
            )
        } {
            unavailable(format!("Media CPU priority unavailable: {e}"));
        }
        if active && p.mode == PerformanceMode::Maximum {
            // These are dedicated application-owned workers with Windows-managed
            // power policy. Restore that baseline when leaving Maximum/recording.
            self.power = true;
            let next = THREAD_POWER_THROTTLING_STATE {
                Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: 0,
            };
            if let Err(e) = set_power(&next) {
                unavailable(e);
            }
        } else {
            self.restore();
        }
    }
    fn restore(&mut self) {
        if self.power {
            self.power = false;
            let baseline = THREAD_POWER_THROTTLING_STATE {
                Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: 0,
                StateMask: 0,
            };
            if let Err(e) = set_power(&baseline) {
                unavailable(e);
            }
        }
    }
}
fn set_power(p: &THREAD_POWER_THROTTLING_STATE) -> Result<(), String> {
    // SAFETY: same-thread guard, valid read-only structure of the documented size.
    unsafe {
        SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            p as *const _ as *const _,
            std::mem::size_of_val(p) as u32,
        )
    }
    .map_err(|e| format!("Thread power policy failed: {e}"))
}
impl Drop for ThreadPolicy {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Pressure must persist for three telemetry intervals; recovery takes twelve.
/// Only preview copies are reduced. Recording settings and encoder parameters stay intact.
#[derive(Default)]
pub struct PreviewBudget {
    last_drops: u64,
    pressure: u8,
    healthy: u8,
    constrained: bool,
}
impl PreviewBudget {
    pub fn observe(&mut self, policy: PerformancePolicy, drops: Option<u64>, queued: bool) -> u32 {
        let busy = drops.is_some_and(|n| n > self.last_drops) || queued;
        if let Some(n) = drops {
            self.last_drops = n;
        }
        if busy {
            self.pressure = self.pressure.saturating_add(1);
            self.healthy = 0;
        } else {
            self.pressure = 0;
            self.healthy = self.healthy.saturating_add(1);
        }
        if policy.adaptive && self.pressure >= 3 {
            self.constrained = true;
        }
        if !policy.adaptive || self.healthy >= 12 {
            self.constrained = false;
        }
        if self.constrained {
            policy.preview_hz().min(10)
        } else {
            policy.preview_hz()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_native_media_policy_is_scoped_to_a_dedicated_thread() {
        std::thread::spawn(|| {
            configure(PerformancePolicy {
                mode: PerformanceMode::Maximum,
                cpu: CpuPriority::High,
                adaptive: false,
            });
            set_active(true);
            // SAFETY: querying this test's own dedicated thread; no other process/thread is changed.
            let before = unsafe { GetThreadPriority(GetCurrentThread()) };
            let Some(mut guard) = crate::WindowsMediaScheduling::join() else {
                println!("Native policy unsupported: {:?}", failure());
                set_active(false);
                configure(Default::default());
                return;
            };
            guard.refresh();
            println!("Maximum + MMCSS High API result: {:?}", failure());
            assert!(failure().is_none(), "{:?}", failure());
            set_active(false);
            guard.refresh();
            drop(guard);
            // SAFETY: query after the same-thread MMCSS guard has reverted membership.
            let after = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(
                before, after,
                "Media scheduling must restore the worker's prior priority."
            );
            assert!(failure().is_none(), "{:?}", failure());
            configure(Default::default());
        })
        .join()
        .unwrap();
    }
    #[test]
    fn revision_adaptive_budget_requires_pressure_recovers_and_preserves_policy() {
        let p = PerformancePolicy {
            adaptive: true,
            ..Default::default()
        };
        let mut b = PreviewBudget::default();
        assert_eq!(b.observe(p, Some(0), false), 30);
        assert_eq!(b.observe(p, Some(1), false), 30);
        assert_eq!(b.observe(p, Some(2), false), 30);
        assert_eq!(b.observe(p, Some(3), false), 10);
        for _ in 0..11 {
            assert_eq!(b.observe(p, Some(3), false), 10);
        }
        assert_eq!(b.observe(p, Some(3), false), 30);
        let off = PerformancePolicy {
            adaptive: false,
            mode: PerformanceMode::Efficiency,
            ..p
        };
        assert_eq!(b.observe(off, Some(10), true), 15);
        let maximum = PerformancePolicy {
            mode: PerformanceMode::Maximum,
            ..off
        };
        assert_eq!(b.observe(maximum, None, false), 60);
        assert_eq!(p.cpu, CpuPriority::Normal);
    }
}
