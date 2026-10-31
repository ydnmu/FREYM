//! Idle-only preview policy; the existing native preview watcher supplies time.
use std::time::{Duration, Instant};
pub(crate) struct PreviewActivity {
    last: Instant,
    pub sleeping: bool,
}
impl Default for PreviewActivity {
    fn default() -> Self {
        Self {
            last: Instant::now(),
            sleeping: false,
        }
    }
}
impl PreviewActivity {
    pub fn interact(&mut self, now: Instant) -> bool {
        self.last = now;
        std::mem::replace(&mut self.sleeping, false)
    }
    pub fn update(&mut self, now: Instant, protected: bool) -> bool {
        if protected {
            return self.interact(now);
        }
        if !self.sleeping && now.saturating_duration_since(self.last) >= Duration::from_secs(30) {
            self.sleeping = true;
            return true;
        }
        false
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn correct_preview_sleeps_only_idle_and_interaction_resets_deadline() {
        let now = Instant::now();
        let mut a = PreviewActivity::default();
        a.interact(now);
        assert!(!a.update(now + Duration::from_secs(29), false));
        assert!(a.update(now + Duration::from_secs(30), false) && a.sleeping);
        assert!(a.interact(now + Duration::from_secs(31)) && !a.sleeping);
        assert!(!a.update(now + Duration::from_secs(60), false));
        assert!(!a.update(now + Duration::from_secs(61), true));
        assert!(!a.update(now + Duration::from_secs(90), false));
        assert!(a.update(now + Duration::from_secs(91), false));
        assert!(a.update(now + Duration::from_secs(92), true) && !a.sleeping);
    }
}
