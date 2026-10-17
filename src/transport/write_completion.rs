//! Complete a transport write only after releasing its input allocation.

use futures::channel::oneshot;
use whatsapp_rust::async_channel::{Sender, TrySendError};
use whatsapp_rust::bytes::Bytes;

#[derive(Debug, PartialEq)]
pub(super) enum ForwardProgress {
    Ready,
    Blocked,
    Closed,
}

/// Retain a full inbound channel's event while the worker services writes.
/// Blocking here could deadlock a reader waiting for its outbound write ACK.
pub(super) fn forward_pending<T>(sender: &Sender<T>, pending: &mut Option<T>) -> ForwardProgress {
    let Some(event) = pending.take() else {
        return ForwardProgress::Ready;
    };
    match sender.try_send(event) {
        Ok(()) => ForwardProgress::Ready,
        Err(TrySendError::Full(event)) => {
            *pending = Some(event);
            ForwardProgress::Blocked
        }
        Err(TrySendError::Closed(_)) => ForwardProgress::Closed,
    }
}

pub(super) type WriteResult = Result<(), String>;

pub(super) struct WriteJob {
    data: Bytes,
    completed: oneshot::Sender<WriteResult>,
}

impl WriteJob {
    pub(super) fn new(data: Bytes) -> (Self, oneshot::Receiver<WriteResult>) {
        let (completed, receiver) = oneshot::channel();
        (Self { data, completed }, receiver)
    }

    pub(super) fn perform<E: std::fmt::Display>(
        self,
        write: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Option<String> {
        let Self { data, completed } = self;
        // Even a cancelled waiter must not skip a frame whose Noise counter
        // was consumed. Shutdown instead retires the entire connection.
        let result = write(&data);
        drop(data);
        // The writer can reclaim its shared BytesMut only after this drop.
        // Sending completion first would race its next buffer reservation.
        let result = result.map_err(|error| error.to_string());
        let failure = result.as_ref().err().cloned();
        let _ = completed.send(result);
        failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::task::{waker, ArcWake};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    #[test]
    fn inbound_backpressure_retains_order_without_blocking_write_completion() {
        let (incoming, receiver) = whatsapp_rust::async_channel::bounded(1);
        incoming.try_send(1).unwrap();
        let mut pending = Some(2);
        assert_eq!(
            forward_pending(&incoming, &mut pending),
            ForwardProgress::Blocked
        );
        let (job, mut completed) = WriteJob::new(Bytes::from_static(b"reply"));
        assert_eq!(job.perform(|_| Ok::<(), &str>(())), None);
        assert_eq!(completed.try_recv().unwrap(), Some(Ok(())));
        assert_eq!(pending, Some(2));
        assert_eq!(receiver.try_recv().unwrap(), 1);
        assert_eq!(
            forward_pending(&incoming, &mut pending),
            ForwardProgress::Ready
        );
        assert_eq!(receiver.try_recv().unwrap(), 2);
        assert!(pending.is_none());
    }

    struct Owner(Arc<AtomicBool>);
    impl AsRef<[u8]> for Owner {
        fn as_ref(&self) -> &[u8] {
            b"frame"
        }
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct ReleasedBeforeWake {
        released: Arc<AtomicBool>,
        woken: Arc<AtomicBool>,
    }
    impl ArcWake for ReleasedBeforeWake {
        fn wake_by_ref(this: &Arc<Self>) {
            assert!(
                this.released.load(Ordering::SeqCst),
                "payload retained when completion woke its sender"
            );
            this.woken.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn success_and_failure_release_payload_before_notifying_sender() {
        for fail in [false, true] {
            let released = Arc::new(AtomicBool::new(false));
            let woken = Arc::new(AtomicBool::new(false));
            let (job, mut completed) = WriteJob::new(Bytes::from_owner(Owner(released.clone())));
            let wake = waker(Arc::new(ReleasedBeforeWake {
                released: released.clone(),
                woken: woken.clone(),
            }));
            let mut context = Context::from_waker(&wake);
            assert!(Pin::new(&mut completed).poll(&mut context).is_pending());
            let failure = job.perform(|data| {
                assert_eq!(data, b"frame");
                assert!(!released.load(Ordering::SeqCst));
                if fail {
                    Err("write failed")
                } else {
                    Ok(())
                }
            });
            assert_eq!(failure.is_some(), fail);
            assert!(woken.load(Ordering::SeqCst));
            let Poll::Ready(Ok(result)) = Pin::new(&mut completed).poll(&mut context) else {
                panic!("write must complete its waiter");
            };
            assert_eq!(result.is_err(), fail);
        }
    }

    #[test]
    fn completion_makes_the_shared_noise_allocation_reclaimable() {
        let mut buffer = whatsapp_rust::bytes::BytesMut::with_capacity(4096);
        let pointer = buffer.as_ptr();
        buffer.resize(1024, 7);
        let (job, mut completed) = WriteJob::new(buffer.split().freeze());
        assert!(!buffer.try_reclaim(4096));
        assert_eq!(job.perform(|_| Ok::<(), &str>(())), None);
        assert_eq!(completed.try_recv().unwrap(), Some(Ok(())));
        assert!(buffer.try_reclaim(4096));
        assert_eq!(buffer.as_ptr(), pointer);
    }

    #[test]
    fn cancelled_waiter_does_not_skip_its_already_numbered_frame() {
        let (job, completed) = WriteJob::new(Bytes::from_static(b"frame"));
        drop(completed);
        let mut written = false;
        assert_eq!(
            job.perform(|_| {
                written = true;
                Ok::<(), &str>(())
            }),
            None
        );
        assert!(written);
    }

    #[test]
    fn abandoned_job_does_not_report_success() {
        let (job, mut completed) = WriteJob::new(Bytes::from_static(b"frame"));
        drop(job);
        assert!(completed.try_recv().is_err());
    }
}
