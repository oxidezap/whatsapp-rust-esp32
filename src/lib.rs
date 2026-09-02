//! ESP-IDF platform implementations for running
//! [`whatsapp-rust`](https://crates.io/crates/whatsapp-rust) on Espressif
//! ESP32 microcontrollers (ESP32-S3 and ESP32-C5).
//!
//! `whatsapp-rust` is platform-agnostic: the protocol engine (Noise, Signal,
//! framing, app state) is written against four traits, and a platform supplies
//! them. This crate is the ESP32 platform, and nothing more: there is no
//! wrapper around `Bot`, no ESP32 event type and no extension trait. You write
//! the same `Bot::builder()` code as on a desktop, with these four values
//! plugged in:
//!
//! | Trait (`whatsapp_rust::wacore`) | Implementation | Notes |
//! |---|---|---|
//! | `store::traits::Backend` | [`NvsStore`] | Pairing, Signal state and sync keys in an NVS partition; the rest in RAM. |
//! | `net::TransportFactory` | [`Esp32TransportFactory`] | ESP-IDF mbedTLS + `tungstenite`, driven on its own thread. |
//! | `net::HttpClient` | [`EspHttpClient`] | Streaming HTTP/1.1 over ESP-IDF TLS/TCP with bounded RAM. |
//! | `runtime::Runtime` | [`Esp32Runtime`] | `spawn` / `sleep` / `spawn_blocking` on an [`Esp32Executor`]. |
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use whatsapp_rust::bot::Bot;
//! use whatsapp_rust::prelude::MessageExt as _;
//! use whatsapp_esp32::{Esp32Runtime, Esp32TransportFactory, EspHttpClient, NvsStore};
//!
//! # fn example() -> anyhow::Result<()> {
//! // Bring up WiFi, SNTP and whatever else the board needs first (see src/main.rs).
//!
//! // The store is flash-backed: it needs a `wa_store` NVS partition in partitions.csv.
//! let store = Arc::new(NvsStore::open_default()?);
//! // One runtime, one executor. The runtime is cheap to clone; clone it per Bot.
//! let (runtime, executor) = Esp32Runtime::create_default()?;
//!
//! // Runs on the calling thread until the future completes, so give this thread
//! // a big (PSRAM) stack and let the future supervise the bot forever.
//! executor.block_on(async move {
//!     let bot = Bot::builder()
//!         .with_backend_arc(store)
//!         .with_transport_factory(Esp32TransportFactory::default())
//!         .with_http_client(EspHttpClient::default())
//!         .with_runtime(runtime.clone())
//!         // 50 instead of the 812 default: generating 812 X25519 keypairs at
//!         // login exhausts internal DRAM.
//!         .with_wanted_pre_key_count(50)
//!         // History is not persisted here, and its sync churns ~14 MB.
//!         .skip_history_sync()
//!         .on_qr_code(|code, timeout| async move {
//!             log::info!("scan this QR (valid for {timeout:?}): {code}");
//!         })
//!         .on_message(|ctx| async move {
//!             if ctx.message.text_content() == Some("ping") {
//!                 let _ = ctx.reply("pong").await;
//!             }
//!         })
//!         .build()
//!         .await?;
//!     bot.run().await;
//!     Ok::<(), anyhow::Error>(())
//! })
//! # }
//! ```
//!
//! Everything in that example except the four `with_*` values and
//! `executor.block_on` is documented by `whatsapp-rust` itself.
//!
//! # What the firmware around it has to provide
//!
//! - **PSRAM**, and a large stack for the executor thread: the demo gives it
//!   256 KB from PSRAM. Install [`psram_alloc::PsramAllocator`] as the global
//!   allocator to keep the Rust heap out of internal DRAM as well.
//! - **A `wa_store` NVS partition** (1 MB in the demo's `partitions.csv`), or
//!   any other name passed to [`NvsStore::open`].
//! - **`sdkconfig.defaults`** along the lines of the demo's: PSRAM enabled for
//!   `malloc` and for task stacks (`CONFIG_SPIRAM_USE_MALLOC`,
//!   `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM`), mbedTLS allocating from
//!   external memory, and the task watchdog not checking the idle task on the
//!   executor's core (the [`BlockingWorker`] runs below idle priority there).
//! - **Time**: the Noise handshake needs a roughly correct clock, so start SNTP
//!   (or set the time some other way) before the first connect.
//!
//! The optional [`admin`] module (feature `admin`, on by default) is the demo's
//! HTTP dashboard; [`supervisor`], [`metrics`] and [`crash`] are the
//! bookkeeping it and the demo firmware share. None of them is needed to run a
//! `Bot`.

pub mod crash;
pub mod http_client;
pub mod metrics;
pub mod psram_alloc;
pub mod runtime;
pub mod storage;
pub mod supervisor;
pub mod transport;

#[cfg(feature = "admin")]
pub mod admin;

pub use http_client::EspHttpClient;
pub use runtime::{BlockingWorker, BoxedTask, Esp32Executor, Esp32Runtime};
pub use storage::{NvsStore, StoreStats, DEFAULT_PARTITION_NAME};
pub use transport::{Esp32Transport, Esp32TransportFactory, EspTlsStream, CONNECT_TIMEOUT};
