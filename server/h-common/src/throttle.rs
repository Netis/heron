//! Log throttling primitive shared across stages.
//!
//! Use when an event (malformed input, flush failure, transient IO error) can
//! recur at high rate and would otherwise flood the log. Call [`ThrottledWarn::tick`]
//! once per occurrence; when it returns `Some(suppressed)`, emit a single log
//! line — `suppressed` is the number of events silently dropped during the
//! preceding window (0 for the first event in a fresh window).
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use h_common::throttle::ThrottledWarn;
//!
//! let mut throttle = ThrottledWarn::new(Duration::from_secs(5));
//! // On every error occurrence:
//! if let Some(suppressed) = throttle.tick() {
//!     if suppressed > 0 {
//!         tracing::warn!(suppressed, "flush failed (latest of many)");
//!     } else {
//!         tracing::warn!("flush failed");
//!     }
//! }
//! ```

use std::time::{Duration, Instant};

/// Tracks whether a repeated event should emit a log line now or be suppressed.
///
/// Not `Sync` — each stage worker should own its own instance. `Clone` is
/// deliberately not derived so copies don't silently diverge.
#[derive(Debug)]
pub struct ThrottledWarn {
    interval: Duration,
    last_emit: Option<Instant>,
    suppressed: u64,
}

impl ThrottledWarn {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_emit: None,
            suppressed: 0,
        }
    }

    /// Call once per event. Returns `Some(suppressed)` if the caller should
    /// emit a log line, or `None` to stay silent. `suppressed` counts the
    /// events dropped since the last emit (0 on the first emit of a window).
    pub fn tick(&mut self) -> Option<u64> {
        self.tick_at(Instant::now())
    }

    /// Same as [`tick`](Self::tick) but with an injectable clock for tests.
    pub fn tick_at(&mut self, now: Instant) -> Option<u64> {
        match self.last_emit {
            None => {
                self.last_emit = Some(now);
                Some(0)
            }
            Some(t) if now.duration_since(t) >= self.interval => {
                self.last_emit = Some(now);
                let suppressed = self.suppressed;
                self.suppressed = 0;
                Some(suppressed)
            }
            Some(_) => {
                self.suppressed = self.suppressed.saturating_add(1);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_tick_emits_with_zero_suppressed() {
        let mut t = ThrottledWarn::new(Duration::from_secs(5));
        assert_eq!(t.tick_at(Instant::now()), Some(0));
    }

    #[test]
    fn subsequent_ticks_within_window_are_suppressed() {
        let mut t = ThrottledWarn::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(t.tick_at(t0), Some(0));
        assert_eq!(t.tick_at(t0 + Duration::from_millis(100)), None);
        assert_eq!(t.tick_at(t0 + Duration::from_secs(4)), None);
    }

    #[test]
    fn window_expiry_emits_with_suppressed_count() {
        let mut t = ThrottledWarn::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(t.tick_at(t0), Some(0));
        for i in 1..=3 {
            assert_eq!(t.tick_at(t0 + Duration::from_millis(i * 100)), None);
        }
        assert_eq!(t.tick_at(t0 + Duration::from_secs(5)), Some(3));
    }

    #[test]
    fn multiple_windows_reset_suppressed_between_emits() {
        let mut t = ThrottledWarn::new(Duration::from_secs(1));
        let t0 = Instant::now();
        assert_eq!(t.tick_at(t0), Some(0));
        assert_eq!(t.tick_at(t0 + Duration::from_millis(500)), None);
        assert_eq!(t.tick_at(t0 + Duration::from_secs(1)), Some(1));
        assert_eq!(t.tick_at(t0 + Duration::from_millis(1_500)), None);
        assert_eq!(t.tick_at(t0 + Duration::from_secs(2)), Some(1));
    }

    #[test]
    fn at_exactly_interval_boundary_emits() {
        let mut t = ThrottledWarn::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(t.tick_at(t0), Some(0));
        assert_eq!(t.tick_at(t0 + Duration::from_secs(5)), Some(0));
    }
}
