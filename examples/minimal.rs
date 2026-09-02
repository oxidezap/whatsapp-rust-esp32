//! The smallest firmware on the library: the same `Bot::builder()` code as on a
//! desktop, with the four ESP32 platform values plugged in.
//!
//! Compiled by CI (`cargo check --release --examples`) so it cannot rot; it does
//! not bring the network up, so it is not a runnable image. `src/main.rs` is the
//! complete firmware (WiFi, SNTP, supervisor, dashboard).

use std::sync::Arc;

use whatsapp_esp32::runtime::spawn_thread;
use whatsapp_esp32::{Esp32Executor, Esp32Runtime, Esp32TransportFactory, EspHttpClient, NvsStore};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::prelude::MessageExt as _;

// The Rust heap goes to PSRAM; internal DRAM stays free for FreeRTOS and mbedTLS.
#[global_allocator]
static ALLOCATOR: whatsapp_esp32::psram_alloc::PsramAllocator =
    whatsapp_esp32::psram_alloc::PsramAllocator;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Bring WiFi and SNTP up here: the Noise handshake needs a roughly correct
    // clock, and the socket needs a route. See `bring_up_wifi` in src/main.rs.

    // Flash-backed: needs a `wa_store` NVS partition (see partitions.csv).
    let store = Arc::new(NvsStore::open_default()?);
    // One runtime, one executor. The runtime is cheap to clone; clone it per Bot.
    let (runtime, executor) = Esp32Runtime::create_default()?;

    // The executor needs a large stack (the send path has deep frames), which
    // only PSRAM can afford; `default_thread_config` is 256 KB there.
    let main_thread = spawn_thread(&Esp32Executor::default_thread_config(), move || {
        executor.block_on(async move {
            let bot = Bot::builder()
                .with_backend_arc(store)
                .with_transport_factory(Esp32TransportFactory::default())
                .with_http_client(EspHttpClient::default())
                .with_runtime(runtime.clone())
                // 812 X25519 keypairs at login (the default) exhaust internal DRAM.
                .with_wanted_pre_key_count(50)
                // History is not persisted here, and its sync churns ~14 MB.
                .skip_history_sync()
                .on_qr_code(|code, timeout| async move {
                    log::info!("scan this QR (valid for {timeout:?}): {code}");
                })
                .on_message(|ctx| async move {
                    if ctx.message.text_content() == Some("ping") {
                        if let Err(error) = ctx.reply("pong").await {
                            log::error!("reply failed: {error}");
                        }
                    }
                })
                .build()
                .await;
            match bot {
                // Runs until logout or `Client::disconnect`; a firmware normally
                // loops here and rebuilds the bot (see `run_whatsapp` in src/main.rs).
                Ok(bot) => bot.run().await,
                Err(error) => log::error!("could not build the bot: {error}"),
            }
        })
    })?;
    main_thread
        .join()
        .map_err(|_| anyhow::anyhow!("executor thread panicked"))
}
