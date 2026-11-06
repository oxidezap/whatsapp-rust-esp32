//! Bounded preference for an idle SDK before a dashboard send.

use std::time::{Duration, Instant};

pub(super) struct QuietPeriod {
    started: Instant,
    quiet_since: Option<Instant>,
}

impl QuietPeriod {
    /// Start a grace period without reserving transient-memory admission.
    pub(super) fn new(started: Instant) -> Self {
        Self {
            started,
            quiet_since: None,
        }
    }

    /// Prefer one continuous quiet second, or stop deferring after five seconds.
    /// The caller must acquire the shared memory gate before actually sending.
    pub(super) fn ready(&mut self, now: Instant, quiet: bool) -> bool {
        if now.duration_since(self.started) >= Duration::from_secs(5) {
            return true;
        }
        if quiet {
            let since = self.quiet_since.get_or_insert(now);
            now.duration_since(*since) >= Duration::from_secs(1)
        } else {
            self.quiet_since = None;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A send is admitted after a complete second without SDK activity.
    #[test]
    fn waits_for_a_full_quiet_second() {
        let start = Instant::now();
        let mut period = QuietPeriod::new(start);
        assert!(!period.ready(start, true));
        assert!(!period.ready(start + Duration::from_millis(999), true));
        assert!(period.ready(start + Duration::from_secs(1), true));
    }

    /// New traffic resets the quiet interval, but not the overall deadline.
    #[test]
    fn activity_restarts_the_quiet_second() {
        let start = Instant::now();
        let mut period = QuietPeriod::new(start);
        assert!(!period.ready(start, true));
        assert!(!period.ready(start + Duration::from_millis(900), false));
        assert!(!period.ready(start + Duration::from_secs(1), true));
        assert!(!period.ready(start + Duration::from_millis(1999), true));
        assert!(period.ready(start + Duration::from_secs(2), true));
    }

    /// Continuous inbound work cannot consume the dashboard's whole timeout.
    #[test]
    fn sustained_traffic_stops_deferring_after_five_seconds() {
        let start = Instant::now();
        let mut period = QuietPeriod::new(start);
        for tick in 0..50 {
            assert!(!period.ready(start + Duration::from_millis(tick * 100), false));
        }
        assert!(period.ready(start + Duration::from_secs(5), false));
    }
}
