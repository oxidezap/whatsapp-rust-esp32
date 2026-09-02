//! ESP-IDF platform implementations for running
//! [`whatsapp-rust`](https://crates.io/crates/whatsapp-rust) on Espressif
//! ESP32 microcontrollers (ESP32-S3, ESP32-C5 and ESP32-C3).
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
//! `examples/minimal.rs`, which CI compiles for the target:
//!
//! ```no_run
#![doc = include_str!("../examples/minimal.rs")]
//! ```
//!
//! Everything in it except the four `with_*` values, the executor thread and
//! the allocator is documented by `whatsapp-rust` itself.
//!
//! # What the firmware around it has to provide
//!
//! - **A large stack for the executor thread**, which on a board with PSRAM
//!   means PSRAM: [`Esp32Executor::default_thread_config`] takes 256 KB there,
//!   and 64 KB of internal DRAM on a chip without it. Where there is PSRAM,
//!   install [`psram_alloc::PsramAllocator`] as the global allocator to keep the
//!   Rust heap out of internal DRAM as well; where there is not, the plain
//!   ESP-IDF allocator is the only heap there is. Both choices follow
//!   [`runtime::HAS_PSRAM`], which is `CONFIG_SPIRAM` from the sdkconfig, so
//!   neither is a decision a consumer has to repeat per chip.
//! - **A `wa_store` NVS partition** (1 MB in the demo's `partitions.csv`), or
//!   any other name passed to [`NvsStore::open`].
//! - **`sdkconfig.defaults`** along the lines of the demo's: the task watchdog
//!   not checking the idle task on the executor's core (the [`BlockingWorker`]
//!   runs below idle priority there), and, on a board with PSRAM, that PSRAM
//!   enabled for `malloc` and for task stacks (`CONFIG_SPIRAM_USE_MALLOC`,
//!   `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM`) with mbedTLS allocating from
//!   external memory. The demo keeps the second group in its own
//!   `sdkconfig.psram`, layered only for the boards that have it.
//! - **Time**: the Noise handshake needs a roughly correct clock, so start SNTP
//!   (or set the time some other way) before the first connect.
//!
//! The optional [`admin`] module (feature `admin`, the only default) is the
//! demo's HTTP dashboard; [`supervisor`], [`metrics`] and [`crash`] are the
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
