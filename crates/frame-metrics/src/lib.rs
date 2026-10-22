//! A nonblocking bounded queue primitive. Synchronization belongs to the owner;
//! native workers must never wait on a lock held by the UI.
use std::{collections::VecDeque, num::NonZeroUsize};

mod runtime;
mod sampler;
pub use runtime::{PipelineCounters, PipelineMetrics, PipelineObservation, PipelineRates};
pub use sampler::{MIN_SAMPLE_WINDOW, RateSample, RateSampler};

#[derive(Debug, Clone, Copy)]
pub enum OverflowPolicy {
    DropNewest,
    DropOldest,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub depth: usize,
    pub capacity: usize,
    pub high_water: usize,
    pub dropped: u64,
}
pub struct BoundedQueue<T> {
    items: VecDeque<T>,
    policy: OverflowPolicy,
    stats: QueueStats,
}
impl<T> BoundedQueue<T> {
    pub fn new(capacity: NonZeroUsize, policy: OverflowPolicy) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity.get()),
            policy,
            stats: QueueStats {
                capacity: capacity.get(),
                ..QueueStats::default()
            },
        }
    }
    /// Returns the dropped item so the caller can release native resources.
    pub fn push(&mut self, item: T) -> Option<T> {
        let dropped = if self.items.len() == self.stats.capacity {
            self.stats.dropped = self.stats.dropped.saturating_add(1);
            match self.policy {
                OverflowPolicy::DropNewest => return Some(item),
                OverflowPolicy::DropOldest => self.items.pop_front(),
            }
        } else {
            None
        };
        self.items.push_back(item);
        self.stats.depth = self.items.len();
        self.stats.high_water = self.stats.high_water.max(self.stats.depth);
        dropped
    }
    pub fn pop(&mut self) -> Option<T> {
        let value = self.items.pop_front();
        self.stats.depth = self.items.len();
        value
    }
    pub fn stats(&self) -> QueueStats {
        self.stats
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_never_grows_or_hides_drops() {
        for policy in [OverflowPolicy::DropOldest, OverflowPolicy::DropNewest] {
            let mut q = BoundedQueue::new(NonZeroUsize::new(2).unwrap(), policy);
            assert_eq!(q.push(1), None);
            q.push(2);
            assert!(q.push(3).is_some());
            assert_eq!(
                q.stats(),
                QueueStats {
                    depth: 2,
                    capacity: 2,
                    high_water: 2,
                    dropped: 1
                }
            );
            assert_eq!(
                q.pop(),
                Some(match policy {
                    OverflowPolicy::DropOldest => 2,
                    OverflowPolicy::DropNewest => 1,
                })
            );
        }
    }
}
