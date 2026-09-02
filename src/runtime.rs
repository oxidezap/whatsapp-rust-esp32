use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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

pub type BoxedTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct BlockingJob {
    run: Box<dyn FnOnce() + Send + 'static>,
    done: async_channel::Sender<()>,
    queued_at: Instant,
}

/// One process-lifetime worker thread for upstream `spawn_blocking` calls:
/// prekey batch generation, app-state mutation decode, anything CPU-bound that
/// whatsapp-rust does not want on the event loop.
///
/// Before this existed those jobs ran inline on the executor thread, so every
/// one of them froze the whole client (and the task watchdog kept count) for
/// its duration. A single worker with a one-slot queue is the smallest fix: no
/// pool to keep stacks for, no way for a reconnect storm to pile up canceled
/// work, and the executor keeps polling the transport while the keys grind.
#[derive(Clone)]
pub struct BlockingWorker {
    jobs: async_channel::Sender<BlockingJob>,
}

impl BlockingWorker {
    pub fn start() -> anyhow::Result<Self> {
        let (jobs, receiver) = async_channel::bounded::<BlockingJob>(1);
        esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
            name: Some(c"wa-blocking"),
            stack_size: 32 * 1024,
            // Below the network/executor threads (priority 5) so a long key
            // batch never delays a socket read; above idle, which the task
            // watchdog no longer watches on this core for exactly this reason
            // (see sdkconfig.defaults).
            priority: 1,
            inherit: false,
            pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
            stack_alloc_caps: enumset::enum_set!(
                esp_idf_svc::hal::task::thread::MallocCap::Spiram
                    | esp_idf_svc::hal::task::thread::MallocCap::Cap8bit
            ),
        }
        .set()?;
        std::thread::Builder::new()
            .name("wa-blocking".to_string())
            .stack_size(32 * 1024)
            .spawn(move || {
                while let Ok(job) = receiver.recv_blocking() {
                    // Dropping the awaiting future cancels work that has not
                    // started. A closure already running cannot be cancelled.
                    if job.done.is_closed() {
                        continue;
                    }
                    let queue_time = job.queued_at.elapsed();
                    let started = Instant::now();
                    (job.run)();
                    let elapsed = started.elapsed();
                    if elapsed >= Duration::from_millis(100) {
                        log::info!(
                            "Blocking job completed in {elapsed:.2?} (queued {queue_time:.2?})"
                        );
                    }
                    let _ = job.done.send_blocking(());
                }
            })?;
        Ok(Self { jobs })
    }

    async fn execute(&self, run: Box<dyn FnOnce() + Send + 'static>) {
        let (done, completed) = async_channel::bounded(1);
        if self
            .jobs
            .send(BlockingJob {
                run,
                done,
                queued_at: Instant::now(),
            })
            .await
            .is_err()
        {
            // The worker thread is process-lifetime; losing it is a bug, not a
            // condition to recover from, but a hung future is worse than a
            // logged one.
            log::error!("WhatsApp blocking worker stopped; job dropped");
            return;
        }
        if completed.recv().await.is_err() {
            log::error!("WhatsApp blocking worker dropped a job's completion");
        }
    }
}

pub struct Esp32Runtime {
    /// Futures spawned from any thread are queued here; the executor loop drains
    /// it via `recv().await`, so a spawn unparks a parked executor.
    task_tx: async_channel::Sender<BoxedTask>,
    timer_service: EspTaskTimerService,
    blocking_worker: BlockingWorker,
}

impl Esp32Runtime {
    pub fn new(
        task_tx: async_channel::Sender<BoxedTask>,
        timer_service: EspTaskTimerService,
        blocking_worker: BlockingWorker,
    ) -> Self {
        Self {
            task_tx,
            timer_service,
            blocking_worker,
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

    /// Hands `f` to the `wa-blocking` thread and parks the calling task until
    /// it is done, so the executor keeps serving the transport meanwhile.
    fn spawn_blocking(
        &self,
        f: Box<dyn FnOnce() + Send + 'static>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let worker = self.blocking_worker.clone();
        Box::pin(async move {
            worker.execute(f).await;
        })
    }

    fn yield_now(&self) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        Some(Box::pin(YieldNow(false)))
    }

    fn yield_frequency(&self) -> u32 {
        1
    }
}
