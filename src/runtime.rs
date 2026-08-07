use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use esp_idf_svc::timer::EspTaskTimerService;
use whatsapp_rust::async_channel;
use whatsapp_rust::async_trait;
use whatsapp_rust::wacore::runtime::{AbortHandle, Runtime};

/// Returns Pending once to give other ready tasks a turn, then Ready.
///
/// The self-wake is intentional and does NOT busy-spin: it is only awaited
/// periodically inside tight loops (see `yield_frequency`), and the surrounding
/// task still parks on real I/O waits (transport recv / `sleep`) once its work
/// is drained.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Wraps a future so it can be aborted via an AtomicBool flag.
struct AbortableFuture<F> {
    inner: F,
    aborted: Arc<AtomicBool>,
}

impl<F: Future<Output = ()> + Unpin> Future for AbortableFuture<F> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.aborted.load(Ordering::Relaxed) {
            return Poll::Ready(());
        }
        Pin::new(&mut self.inner).poll(cx)
    }
}

type BoxedTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub struct Esp32Runtime {
    /// Futures spawned from any thread are queued here; the executor loop drains
    /// it via `recv().await`, so a spawn unparks a parked executor.
    task_tx: async_channel::Sender<BoxedTask>,
    timer_service: EspTaskTimerService,
}

impl Esp32Runtime {
    pub fn new(
        task_tx: async_channel::Sender<BoxedTask>,
        timer_service: EspTaskTimerService,
    ) -> Self {
        Self {
            task_tx,
            timer_service,
        }
    }
}

#[async_trait]
impl Runtime for Esp32Runtime {
    fn spawn(&self, future: BoxedTask) -> AbortHandle {
        let aborted = Arc::new(AtomicBool::new(false));
        let aborted_clone = aborted.clone();

        let abortable = Box::pin(AbortableFuture {
            inner: future,
            aborted: aborted_clone,
        });

        // Unbounded channel: try_send only fails if the executor is gone.
        if self.task_tx.try_send(abortable).is_err() {
            log::error!("Failed to spawn task: executor channel closed");
        }

        AbortHandle::new(move || {
            aborted.store(true, Ordering::Relaxed);
        })
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // EspAsyncTimer arms a real one-shot esp_timer and parks the task on a
        // waker-backed Notification, so the executor sleeps the CPU instead of
        // busy-polling a deadline. This is the official esp-idf-svc async timer;
        // it cancels + deletes the timer on drop (so a dropped/raced sleep, e.g.
        // the loser of a `timeout()`/`select`, cleans up).
        match self.timer_service.timer_async() {
            Ok(mut timer) => Box::pin(async move {
                if let Err(e) = timer.after(duration).await {
                    log::error!("esp_timer sleep failed: {e}");
                }
            }),
            Err(e) => {
                // Fail open: complete immediately rather than hang the task.
                log::error!("failed to create async timer ({e}); not sleeping");
                Box::pin(async {})
            }
        }
    }

    /// Runs `f` INLINE on the executor thread. There is no thread pool to offload
    /// to: the chip has one usable core for this workload and each FreeRTOS thread
    /// costs a fixed stack, so a pool would trade a stall for permanent RAM.
    ///
    /// The consequence is that every upstream `spawn_blocking` (prekey batch
    /// generation, history-sync blob decode, appstate mutation decode) blocks the
    /// whole executor for its duration. That is why `with_wanted_pre_key_count`
    /// is lowered and `skip_history_sync()` is set in main.rs — both are there to
    /// keep the inline work short enough for the task watchdog.
    fn spawn_blocking(
        &self,
        f: Box<dyn FnOnce() + Send + 'static>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            f();
        })
    }

    fn yield_now(&self) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        Some(Box::pin(YieldNow(false)))
    }

    fn yield_frequency(&self) -> u32 {
        1
    }
}
