use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
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

/// Whether this ESP-IDF build has external SPI RAM.
///
/// `esp_idf_spiram` is the cfg esp-idf-sys derives from `CONFIG_SPIRAM` in the
/// sdkconfig, so this follows the board's own `sdkconfig.defaults.<mcu>` and is
/// never a guess about which chip is being built for. It decides two things
/// that no `Bot` code has to know about: where thread stacks are allocated
/// ([`stack_caps`]) and how large they can afford to be.
///
/// On an ESP32-S3 or ESP32-C5 there are 8 MB to spend and the defaults are
/// generous. On an ESP32-C3 the whole memory system is ~400 KB of on-chip SRAM,
/// so the same defaults would not leave room for the TLS session, and the
/// sizes below come down accordingly.
pub const HAS_PSRAM: bool = cfg!(esp_idf_spiram);

/// The heap a thread stack is allocated from: PSRAM where the build has it,
/// internal DRAM otherwise.
///
/// Asking for `MallocCap::Spiram` on a chip without PSRAM does not fall back,
/// it fails the spawn, so this is not a preference but a requirement.
pub fn stack_caps() -> enumset::EnumSet<MallocCap> {
    if HAS_PSRAM {
        enumset::enum_set!(MallocCap::Spiram | MallocCap::Cap8bit)
    } else {
        enumset::enum_set!(MallocCap::Internal | MallocCap::Cap8bit)
    }
}

/// Pick a buffer or stack size by whether this build has PSRAM.
///
/// One function so that every size that has to shrink on a chip with no external
/// RAM -- the thread stacks here, the transport's WebSocket buffers -- reads the
/// same way, and so the no-PSRAM column can be found and re-tuned in one place.
/// Public because a firmware sizing its own threads faces the same choice.
///
/// `GET /metrics` reports `stack_*_min` for each of this crate's threads, which
/// is where those numbers came from and where a re-tune should start.
pub const fn by_ram(psram: usize, no_psram: usize) -> usize {
    if HAS_PSRAM {
        psram
    } else {
        no_psram
    }
}

/// Spawn a `std` thread whose FreeRTOS task has `config`'s name, stack size,
/// stack memory, priority and core.
///
/// `ThreadSpawnConfiguration::set` is sticky: it configures every thread the
/// calling thread spawns from then on. This applies it for the one spawn and
/// restores what was there before, so a caller's own thread configuration is
/// never silently replaced by a worker's.

pub fn spawn_thread(
    config: &ThreadSpawnConfiguration,
    f: impl FnOnce() + Send + 'static,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let previous = ThreadSpawnConfiguration::get();
    config.set()?;
    let mut builder = std::thread::Builder::new().stack_size(config.stack_size);
    if let Some(name) = config.name {
        builder = builder.name(name.to_string_lossy().into_owned());
    }
    let handle = builder.spawn(f)?;
    // The thread is already running, so this cannot be a failure of the spawn;
    // the caller's next spawn would just inherit `config`.
    if let Err(error) = previous.unwrap_or_default().set() {
        log::warn!("could not restore the thread configuration after spawning: {error}");
    }
    Ok(handle)
}

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
    /// The thread [`BlockingWorker::start`] creates: 32 KB of stack (14 KB
    /// without PSRAM, see [`HAS_PSRAM`]), pinned to core 0 below the network
    /// and executor threads (priority 5) so a long key batch never delays a
    /// socket read, and above idle, which the task watchdog must therefore not
    /// check on that core.
    pub fn default_thread_config() -> ThreadSpawnConfiguration {
        ThreadSpawnConfiguration {
            name: Some(c"wa-blocking"),
            stack_size: by_ram(32 * 1024, 14 * 1024),
            priority: 1,
            inherit: false,
            pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
            stack_alloc_caps: stack_caps(),
        }
    }

    /// Start the worker on [`BlockingWorker::default_thread_config`].
    pub fn start() -> anyhow::Result<Self> {
        Self::start_with(Self::default_thread_config())
    }

    /// Start the worker on a thread of the caller's choosing (see
    /// [`spawn_thread`]). The dashboard's `/metrics` reports the stack
    /// high-water mark of the thread named `wa-blocking`; another name is
    /// fine, it just drops out of that report.
    pub fn start_with(config: ThreadSpawnConfiguration) -> anyhow::Result<Self> {
        let (jobs, receiver) = async_channel::bounded::<BlockingJob>(1);
        spawn_thread(&config, move || {
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
                        "Blocking job completed in {elapsed:.2?} (queued {queue_time:.2?}){}",
                        crate::metrics::heap_note()
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

/// The `wacore::runtime::Runtime` for ESP-IDF: spawns onto an [`Esp32Executor`],
/// sleeps on `esp_timer`, and runs `spawn_blocking` jobs on the [`BlockingWorker`].
///
/// Cheap to clone; every clone spawns onto the same executor. Hand a clone to
/// each `Bot` you build (`BotBuilder::with_runtime` takes it by value).
#[derive(Clone)]
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

    /// Create a runtime with its own `esp_timer` service and a
    /// [`BlockingWorker`] on its default thread, paired with the
    /// [`Esp32Executor`] that will run everything spawned on it.
    pub fn create_default() -> anyhow::Result<(Self, Esp32Executor)> {
        Self::create(BlockingWorker::start()?)
    }

    /// Like [`Esp32Runtime::create_default`], with a worker the caller started
    /// (see [`BlockingWorker::start_with`]).
    pub fn create(blocking_worker: BlockingWorker) -> anyhow::Result<(Self, Esp32Executor)> {
        let (task_tx, task_rx) = async_channel::unbounded();
        let timer_service = EspTaskTimerService::new()?;
        Ok((
            Self::new(task_tx, timer_service, blocking_worker),
            Esp32Executor { task_rx },
        ))
    }

    /// The queue [`Runtime::spawn`] pushes onto, for code that must know whether
    /// a spawn was accepted (the admin server refuses a request instead of
    /// silently dropping it once the executor is gone).
    pub fn spawner(&self) -> async_channel::Sender<BoxedTask> {
        self.task_tx.clone()
    }
}

/// The single-threaded executor behind [`Esp32Runtime`]: an
/// `edge_executor::LocalExecutor` that drains the runtime's spawn queue and
/// parks the OS thread whenever every task is waiting on I/O or a timer.
///
/// Created by [`Esp32Runtime::create_default`]; consumed by
/// [`Esp32Executor::block_on`], which is the firmware's main loop.
pub struct Esp32Executor {
    task_rx: async_channel::Receiver<BoxedTask>,
}

impl Esp32Executor {
    /// The thread [`Esp32Executor::block_on`] is meant to run on: `wa-main`,
    /// 256 KB of PSRAM stack (the send path with a quoted reply and an edit has
    /// deep frames), core 0, priority 5. Spawn it with [`spawn_thread`].
    ///
    /// Without PSRAM this is 32 KB of internal DRAM instead, and it is the
    /// single largest concession the ESP32-C3 port makes. That number is
    /// measured, not reasoned: `metrics::log_memory_profile` on the emulated C3
    /// reports 43,932 of the previous 65,536 bytes never touched across the
    /// pairing flow, a 515 reconnect and the prekey upload -- a peak of 21,604,
    /// confirmed a second time at 21,608 once the size was cut. 32 KB keeps 52%
    /// headroom over that while returning 32 KB to a heap whose largest free
    /// block, not whose total, is what decides whether the next allocation
    /// fits. A firmware that adds
    /// deeper work to the executor should watch `stack_wa_main_min` in
    /// `GET /metrics` on that chip, or read the line this crate now logs on
    /// every connect.
    ///
    /// The demo firmware does not spawn this thread without PSRAM: there the
    /// executor runs on the ESP-IDF main task (`CONFIG_ESP_MAIN_TASK_STACK_SIZE`
    /// in `sdkconfig.defaults.esp32c3`), so the 32 KB arm below is what library
    /// consumers who spawn the thread themselves get.
    pub fn default_thread_config() -> ThreadSpawnConfiguration {
        ThreadSpawnConfiguration {
            name: Some(c"wa-main"),
            stack_size: by_ram(256 * 1024, 32 * 1024),
            priority: 5,
            inherit: false,
            pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
            stack_alloc_caps: stack_caps(),
        }
    }

    /// Drive `main` to completion on the calling thread, running every future
    /// spawned through the paired [`Esp32Runtime`] alongside it.
    ///
    /// Returns when `main` completes. Spawned tasks are dropped at that point,
    /// so a firmware's `main` future normally never returns: it supervises the
    /// `Bot` (rebuilding it after an exit) and keeps the `Esp32Runtime` alive,
    /// which is what keeps the spawn queue open.
    ///
    /// Not for the ESP-IDF main task: its stack (`CONFIG_ESP_MAIN_TASK_STACK_SIZE`)
    /// is far too small. Run it on a thread from
    /// [`Esp32Executor::default_thread_config`].
    pub fn block_on<T>(self, main: impl Future<Output = T>) -> T {
        // `UnboundQueue` (a growable VecDeque) rather than the default 64-slot
        // `BoundQueue`: a burst of more than 64 runnable tasks must queue, not
        // fail the spawn and take the firmware down with it.
        let executor: edge_executor::LocalExecutor<'_, edge_executor::UnboundQueue> =
            edge_executor::LocalExecutor::new();
        let task_rx = self.task_rx;

        // `run()` polls `main` plus every spawned task, and PARKS the OS thread
        // whenever all of them are pending (transport recv / esp_timer sleep),
        // unparking when a real waker fires. `recv().await` on the spawn queue
        // is such a waker, so a spawn from another thread unparks it too.
        futures::executor::block_on(executor.run(async {
            let pump = async {
                while let Ok(future) = task_rx.recv().await {
                    executor.spawn(future).detach();
                }
            };
            futures::pin_mut!(main);
            futures::pin_mut!(pump);
            match futures::future::select(main, pump).await {
                futures::future::Either::Left((value, _pump)) => value,
                // Every `Esp32Runtime` was dropped, so nothing can be spawned
                // any more; `main` still owns whatever it is running.
                futures::future::Either::Right(((), main)) => main.await,
            }
        }))
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
