//! Bounded startup grace and cancellable queue admission for dashboard sends.

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

/// Keep one queued acquisition across ticks, dropping it promptly on cancellation.
/// Check again before returning a permit so a timeout cannot start a late send.
pub(super) async fn acquire_cancellable<T, E>(
    acquire: impl std::future::Future<Output = T>,
    cancelled: impl Fn() -> bool,
    ticks: impl futures::Stream<Item = Result<(), E>>,
) -> Result<Option<T>, E> {
    use futures::{future::Either, StreamExt};
    futures::pin_mut!(acquire, ticks);
    loop {
        if cancelled() {
            return Ok(None);
        }
        match futures::future::select(acquire.as_mut(), ticks.next()).await {
            Either::Left((permit, _)) => return Ok((!cancelled()).then_some(permit)),
            Either::Right((Some(result), _)) => result?,
            Either::Right((None, _)) => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::MemoryGate;
    use futures::{executor::block_on, task::noop_waker_ref, FutureExt};
    use std::{
        future::Future,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };

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

    /// A cancelled request leaves the queue even while its owner keeps the gate.
    #[test]
    fn cancellation_does_not_wait_for_the_gate_owner() {
        let gate = MemoryGate::default();
        let owner = block_on(gate.acquire());
        let cancelled = AtomicBool::new(false);
        let (tx, ticks) = futures::channel::mpsc::unbounded::<Result<(), ()>>();
        let mut waiting = Box::pin(acquire_cancellable(
            gate.acquire(),
            || cancelled.load(Ordering::Acquire),
            ticks,
        ));
        let mut cx = Context::from_waker(noop_waker_ref());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        cancelled.store(true, Ordering::Release);
        tx.unbounded_send(Ok(())).unwrap();
        assert!(matches!(
            waiting.as_mut().poll(&mut cx),
            Poll::Ready(Ok(None))
        ));
        drop(waiting);
        drop(owner);
        assert!(gate.acquire().now_or_never().is_some());
    }

    /// Timer ticks must not recreate an acquisition behind a later waiter.
    #[test]
    fn polling_preserves_the_original_queue_position() {
        let gate = MemoryGate::default();
        let owner = block_on(gate.acquire());
        let (tx, ticks) = futures::channel::mpsc::unbounded::<Result<(), ()>>();
        let mut first = Box::pin(acquire_cancellable(gate.acquire(), || false, ticks));
        let mut second = Box::pin(gate.acquire());
        let mut cx = Context::from_waker(noop_waker_ref());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        for _ in 0..3 {
            tx.unbounded_send(Ok(())).unwrap();
            assert!(first.as_mut().poll(&mut cx).is_pending());
        }
        drop(owner);
        let Poll::Ready(Ok(Some(permit))) = first.as_mut().poll(&mut cx) else {
            panic!("the first queued send lost its position");
        };
        assert!(second.as_mut().poll(&mut cx).is_pending());
        drop(permit);
        assert!(second.as_mut().poll(&mut cx).is_ready());
    }

    /// A timeout racing a ready permit must release it instead of sending late.
    #[test]
    fn cancellation_after_acquisition_releases_the_permit() {
        let gate = MemoryGate::default();
        let cancelled = AtomicBool::new(false);
        let acquire = async {
            let permit = gate.acquire().await;
            cancelled.store(true, Ordering::Release);
            permit
        };
        let result = block_on(acquire_cancellable(
            acquire,
            || cancelled.load(Ordering::Acquire),
            futures::stream::pending::<Result<(), ()>>(),
        ));
        assert!(matches!(result, Ok(None)));
        assert!(gate.acquire().now_or_never().is_some());
    }
}
