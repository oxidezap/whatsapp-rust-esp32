mod admin;
mod crash;
mod http_client;
mod metrics;
mod psram_alloc;
mod runtime;
mod storage;
mod transport;

use anyhow::Result;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::peripherals::Peripherals,
    nvs::EspDefaultNvsPartition,
    wifi::{
        AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi, PmfConfiguration,
    },
};
use log::{error, info, warn};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wacore::net::{HttpClient as _, HttpRequest};
use wacore::proto_helpers::MessageExt;
use wacore::types::events::Event;
use waproto::whatsapp as wa;
use whatsapp_rust::bot::{Bot, MessageContext};

use crate::http_client::EspHttpClient;
use crate::runtime::Esp32Runtime;
use crate::storage::MemoryStore;
use crate::transport::Esp32TransportFactory;

// WiFi + server come from .env at build time (see .env.example). option_env! keeps
// a fresh clone with no .env compiling; the firmware reports missing WiFi at runtime.
const WIFI_SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const WIFI_PASS: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

// WebSocket URL of the mock server or WhatsApp gateway. Override via WHATSAPP_WS_URL in .env.
const MOCK_SERVER_WS: &str = match option_env!("WHATSAPP_WS_URL") {
    Some(u) => u,
    None => "wss://192.168.0.4:8080/ws/chat",
};
// Accept the bundled self-signed CA (mock server). Set false for the real WhatsApp gateway.
const SKIP_TLS_VERIFY: bool = true;

// DEV-ONLY auto-pair: when the bot emits a pairing QR, POST it to the bartender mock
// server's `/admin/mock-phone/scan-qr` endpoint, which completes pairing as if a phone
// scanned it (mirrors whatsapp-rust e2e `spawn_qr_autoresponder_http`). Set false for the
// real WhatsApp gateway (no such endpoint exists there; you scan with your phone).
const MOCK_AUTOPAIR: bool = true;

const PING_TRIGGER: &str = "\u{1f980}ping"; // 🦀ping
const PONG_TEXT: &str = "\u{1f3d3} Pong!"; // 🏓 Pong!
const REACTION_EMOJI: &str = "\u{1f3d3}"; // 🏓

/// Stack for the main async executor thread, in PSRAM (so 256 KB is cheap). The full
/// send path (reaction + quoted reply + edit) has deep frames and needs every byte.
const MAIN_TASK_STACK_SIZE: usize = 256 * 1024;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Verbose by design: surface whatsapp-rust's DEBUG decrypt/session/protocol logs
    // on the serial monitor (the compile-time default is INFO). The C log macros are
    // compiled out above INFO, so this only unmasks the Rust-side debug records routed
    // through esp_log_write. It keeps the demo's protocol flow visible on the wire;
    // drop to LevelFilter::Info for a quieter, slightly faster build.
    log::set_max_level(log::LevelFilter::Debug);
    unsafe {
        esp_idf_svc::sys::esp_log_level_set(
            c"*".as_ptr(),
            esp_idf_svc::sys::esp_log_level_t_ESP_LOG_DEBUG,
        );
    }

    // Capture the REAL cause of a Rust panic (location + message) before the runtime
    // aborts. The serial log is gone after reboot, so also persist it to RTC RAM
    // (crash::record_panic) where the next boot can read it. Hardware exceptions
    // (LoadProhibited, etc.) carry no Rust string, so the ESP-IDF core dump records those.
    std::panic::set_hook(Box::new(|info| {
        error!("RUST PANIC: {info}");
        crash::record_panic(info);
    }));

    // Why did the previous boot end? Panic/Watchdog/Brownout here is the real,
    // non-speculative reason the chip reset, not a guess.
    let last_reset = esp_idf_svc::hal::reset::ResetReason::get();
    metrics::set_last_reset(last_reset);
    // The panic message from the run that just crashed (survives the warm reboot).
    metrics::set_last_panic(crash::take_last_panic());
    // Core dump from flash (survives power-off; covers panics AND hw exceptions).
    metrics::set_last_coredump(crash::take_coredump_summary());

    let _eventfs = esp_idf_svc::io::vfs::MountedEventfs::mount(5)?;

    let last_panic = metrics::last_panic_str();
    info!("whatsapp-esp32 starting... (last reset: {last_reset:?}{last_panic})");

    // WIFI_SSID is injected at build time via option_env!; it is legitimately empty
    // in a clone-and-run build with no .env, so this guard is intentional (clippy
    // only sees this build's baked-in value).
    #[allow(clippy::const_is_empty)]
    if WIFI_SSID.is_empty() {
        error!(
            "WiFi is not configured. Copy .env.example to .env, set WIFI_SSID / WIFI_PASS, and reflash."
        );
        return Err(anyhow::anyhow!("WIFI_SSID is empty"));
    }

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
    )?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASS.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        // Advertise PMF support (but don't require it). The embedded-svc default is
        // NotCapable, which modern WPA2/WPA3-mixed APs (mesh / ISP routers) often
        // reject at the association stage. The symptom is auth OK then
        // `assoc -> init` after ~1s, retried forever. capable+!required is the
        // ESP-IDF station example default and is backward-compatible with plain WPA2.
        pmf_cfg: PmfConfiguration::Capable { required: false },
        ..Default::default()
    }))?;

    wifi.start()?;
    // Association can fail transiently (congested 2.4 GHz, AP busy). Retry instead
    // of letting the `?` propagate and return from main() (which halts the chip).
    info!("Connecting to '{}'...", WIFI_SSID);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let r = wifi.connect();
        let r = r.and_then(|()| wifi.wait_netif_up());
        match r {
            Ok(()) => break,
            Err(e) => {
                warn!("WiFi connect attempt {attempt} failed: {e}; retrying in 3s");
                let _ = wifi.disconnect();
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("WiFi connected! IP: {}", ip_info.ip);

    let _sntp = esp_idf_svc::sntp::EspSntp::new_default()?;
    std::thread::sleep(std::time::Duration::from_secs(2));

    // mDNS: reachable at http://esp32-whatsapp.local:8081/dashboard
    let _mdns = {
        let mut mdns = esp_idf_svc::mdns::EspMdns::take()?;
        mdns.set_hostname("esp32-whatsapp")?;
        mdns.set_instance_name("ESP32 WhatsApp")?;
        mdns.add_service(None, "_http", "_tcp", 8081, &[("path", "/dashboard")])?;
        mdns
    };

    let store = std::sync::Arc::new(MemoryStore::new());
    let device_status = std::sync::Arc::new(storage::DeviceStatus::new());
    let _admin_server = admin::start_admin_server(store.clone(), device_status.clone())?;
    info!("Admin: http://esp32-whatsapp.local:8081/dashboard");
    info!("Admin: http://{}:8081/dashboard", ip_info.ip);

    info!(
        "Free heap: {} bytes (internal: {} bytes)",
        unsafe { esp_idf_svc::sys::esp_get_free_heap_size() },
        unsafe { esp_idf_svc::sys::esp_get_free_internal_heap_size() }
    );

    // Configure PSRAM stack for the main async thread
    esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        name: Some(c"wa-main"),
        stack_size: MAIN_TASK_STACK_SIZE,
        priority: 5,
        inherit: false,
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
        stack_alloc_caps: enumset::enum_set!(
            esp_idf_hal::task::thread::MallocCap::Spiram
                | esp_idf_hal::task::thread::MallocCap::Cap8bit
        ),
    }
    .set()?;

    // Channel for Runtime::spawn() -> main executor. async_channel's recv() has a
    // waker, so the executor parks when idle and a spawn unparks it.
    let (task_tx, task_rx) =
        async_channel::unbounded::<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>();

    let jh = std::thread::Builder::new()
        .stack_size(MAIN_TASK_STACK_SIZE)
        .spawn(move || {
            run_executor(task_tx, task_rx, store, device_status);
        })?;

    if let Err(e) = jh.join() {
        error!("Executor thread panicked: {:?}", e);
    }
    Ok(())
}

fn run_executor(
    task_tx: async_channel::Sender<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    task_rx: async_channel::Receiver<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    store: std::sync::Arc<MemoryStore>,
    device_status: std::sync::Arc<storage::DeviceStatus>,
) {
    // With the `unbounded` feature the const generic is nominal: the run-queue is a
    // growable SegQueue, so >64 simultaneously-runnable tasks no longer panic.
    let executor: edge_executor::LocalExecutor<'_, 64> = edge_executor::LocalExecutor::new();

    executor
        .spawn(run_whatsapp(task_tx, store, device_status))
        .detach();

    // run() polls the spawned bot task plus the spawn-pump below, and PARKS the OS
    // thread whenever every task is waiting (transport recv / esp_timer sleep),
    // unparking when a real waker fires. Replaces the old try_tick + yield_now loop
    // that pinned the core at 100%.
    futures::executor::block_on(executor.run(async {
        while let Ok(future) = task_rx.recv().await {
            executor.spawn(future).detach();
        }
    }));
}

async fn run_whatsapp(
    task_tx: async_channel::Sender<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    store: std::sync::Arc<MemoryStore>,
    device_status: std::sync::Arc<storage::DeviceStatus>,
) {
    let timer_service = match esp_idf_svc::timer::EspTaskTimerService::new() {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to create timer service: {e}");
            return;
        }
    };

    // Supervisor loop: the firmware must never let app_main() return (that tears down
    // WiFi/admin and halts the chip). whatsapp-rust reconnects forever internally, but
    // if the bot future ever does end, re-create it after a short delay. This task also
    // keeps `task_tx` alive, so the executor's spawn channel never closes.
    loop {
        match run_whatsapp_inner(
            task_tx.clone(),
            store.clone(),
            device_status.clone(),
            timer_service.clone(),
        )
        .await
        {
            Ok(()) => warn!("WhatsApp client exited; restarting in 5s"),
            Err(e) => error!("WhatsApp client error: {e}; restarting in 5s"),
        }
        if let Ok(mut t) = timer_service.timer_async() {
            let _ = t.after(std::time::Duration::from_secs(5)).await;
        }
    }
}

async fn run_whatsapp_inner(
    task_tx: async_channel::Sender<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    backend: std::sync::Arc<MemoryStore>,
    device_status: std::sync::Arc<storage::DeviceStatus>,
    timer_service: esp_idf_svc::timer::EspTaskTimerService,
) -> Result<()> {
    let transport_factory = Esp32TransportFactory::new(MOCK_SERVER_WS, SKIP_TLS_VERIFY);
    let http_client = EspHttpClient::new(SKIP_TLS_VERIFY);
    let runtime = Esp32Runtime::new(task_tx, timer_service);

    // One-shot guard so dev auto-pair POSTs the QR only once per bot session.
    let scanned = Arc::new(AtomicBool::new(false));

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(transport_factory)
        .with_http_client(http_client)
        .with_runtime(runtime)
        .with_push_name("esp32-test".to_string())
        .with_version((2, 3000, 0))
        // 50 instead of the 812 default: generating 812 X25519 keypairs inline at
        // login blocks the single-core executor ~8s and exhausts internal DRAM.
        // Configurable upstream as of whatsapp-rust PR #695, so no fork needed.
        .with_wanted_pre_key_count(50)
        .on_event(move |event, client| {
            let ds = device_status.clone();
            let scanned = scanned.clone();
            async move {
                match &*event {
                    Event::PairingQrCode { code, timeout } => {
                        info!("QR CODE (valid for {}s)", timeout.as_secs());
                        ds.set_qr_code(code.clone());
                        if MOCK_AUTOPAIR && !scanned.swap(true, Ordering::Relaxed) {
                            auto_scan_qr(code).await;
                        }
                    }
                    Event::Connected(_) => {
                        // Send "active" delivery receipts (type omitted) instead of the
                        // default "inactive" ones a passive companion emits. Without this,
                        // the client ACKs incoming messages with <receipt type="inactive">,
                        // which the server does NOT render as the ✓✓ delivered ticks, so
                        // the sender's message stays on a single ✓. The forced value (2)
                        // survives reconnects (only presence-driven activity, value 1, is
                        // demoted on disconnect), so setting it once per connect is enough.
                        client.set_force_active_delivery_receipts(true);

                        let pn = client.get_pn().await.map(|j| j.to_string());
                        let lid = client.get_lid().await.map(|j| j.to_string());
                        ds.set_connected(pn, lid);
                        info!("Connected to WhatsApp!");
                        info!("Free heap: {} bytes", unsafe {
                            esp_idf_svc::sys::esp_get_free_heap_size()
                        });
                    }
                    Event::LoggedOut(reason) => {
                        info!("Logged out: {:?}", reason);
                    }
                    Event::Message(msg, info) => {
                        let text = msg.text_content().unwrap_or("<no text>");
                        info!("Message from {}: {:?}", info.source.sender, text);
                        if text == PING_TRIGGER {
                            if let Some(ctx) = MessageContext::from_event(&event, client) {
                                handle_message(&ctx).await;
                            }
                        }
                    }
                    Event::UndecryptableMessage(u) => {
                        warn!(
                            "UNDECRYPTABLE from {} id={} fail_mode={:?} unavailable={} type={:?}",
                            u.info.source.sender,
                            u.info.id,
                            u.decrypt_fail_mode,
                            u.is_unavailable,
                            u.unavailable_type
                        );
                    }
                    other => {
                        info!("event: {:?}", other.kind());
                    }
                }
            }
        })
        .build()
        .await?;

    info!("Bot built, starting run loop...");
    let handle = bot.run().await?;
    if let Err(e) = handle.await {
        error!("Bot run loop error: {:?}", e);
    }
    Ok(())
}

/// DEV-ONLY: POST the pairing QR to the bartender mock server's scan-qr admin
/// endpoint, completing pairing without a phone. Derives the admin URL from
/// `MOCK_SERVER_WS` (ws→http / wss→https, path `/admin/mock-phone/scan-qr`),
/// matching whatsapp-rust's e2e helper. The request is blocking HTTP done inline;
/// it briefly stalls the executor once, which is fine for a one-shot pairing step.
async fn auto_scan_qr(code: &str) {
    let (host, port, _path, tls) = match crate::http_client::parse_url(MOCK_SERVER_WS) {
        Ok(v) => v,
        Err(e) => {
            error!("auto-pair: bad mock URL: {e}");
            return;
        }
    };
    let scheme = if tls { "https" } else { "http" };
    let url = format!("{scheme}://{host}:{port}/admin/mock-phone/scan-qr");
    info!("auto-pair: POSTing QR to {url}");
    let http = EspHttpClient::new(SKIP_TLS_VERIFY);
    let req = HttpRequest {
        url,
        method: "POST".into(),
        headers: std::collections::HashMap::new(),
        body: Some(code.as_bytes().to_vec()),
    };
    match http.execute(req).await {
        Ok(r) if (200..300).contains(&r.status_code) => {
            info!("auto-pair: scan-qr accepted (HTTP {})", r.status_code)
        }
        Ok(r) => warn!(
            "auto-pair: scan-qr HTTP {}: {}",
            r.status_code,
            String::from_utf8_lossy(&r.body)
        ),
        Err(e) => error!("auto-pair: scan-qr POST failed: {e}"),
    }
}

async fn handle_message(ctx: &MessageContext) {
    info!("Building reaction message...");

    let key = wa::MessageKey {
        remote_jid: Some(ctx.info.source.chat.to_string()),
        id: Some(ctx.info.id.clone()),
        from_me: Some(ctx.info.source.is_from_me),
        participant: ctx
            .info
            .source
            .is_group
            .then(|| ctx.info.source.sender.to_string()),
    };
    let reaction = wa::Message {
        reaction_message: Some(wa::message::ReactionMessage {
            key: Some(key),
            text: Some(REACTION_EMOJI.to_string()),
            sender_timestamp_ms: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            ),
            ..Default::default()
        }),
        ..Default::default()
    };
    info!("Sending reaction...");
    if let Err(e) = ctx.send_message(reaction).await {
        error!("Failed to send reaction: {}", e);
    }
    info!("Reaction sent! Building pong reply...");

    let start = std::time::Instant::now();
    let context_info = ctx.build_quote_context();
    let reply = wa::Message {
        extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
            text: Some(PONG_TEXT.to_string()),
            context_info: Some(Box::new(context_info)),
            ..Default::default()
        })),
        ..Default::default()
    };

    let sent = match ctx.send_message(reply).await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to send pong: {}", e);
            return;
        }
    };

    let duration = format!("{:.2?}", start.elapsed());
    info!(
        "Send took {}. Editing message {}...",
        duration, &sent.message_id
    );

    let edit = wa::Message {
        extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
            text: Some(format!("{PONG_TEXT}\n`{duration}`")),
            ..Default::default()
        })),
        ..Default::default()
    };
    if let Err(e) = ctx.edit_message(sent.message_id.clone(), edit).await {
        error!("Failed to edit message {}: {}", sent.message_id, e);
    }
}
