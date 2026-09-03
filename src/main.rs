use anyhow::Result;
#[cfg(not(feature = "qemu"))]
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi, PmfConfiguration,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::peripherals::Peripherals,
    nvs::{EspDefaultNvsPartition, EspNvs},
};
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
// Single upstream dependency: whatsapp-rust re-exports wacore/waproto/buffa and
// the shared support crates, so there is no way for them to drift out of sync.
use whatsapp_rust::bytes;
use whatsapp_rust::prelude::{wa, Bot, Event, MessageContext, MessageExt as _, MessageField};
use whatsapp_rust::wacore::net::{HttpClient as _, HttpRequest};
use whatsapp_rust::wacore::runtime::Runtime as _;
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::wacore::types::events::{
    ConnectFailureReason, EventHandler, EventInterest, EventKind,
};

#[cfg(feature = "admin")]
use whatsapp_esp32::admin;
use whatsapp_esp32::runtime::spawn_thread;
use whatsapp_esp32::supervisor::{
    run_maintenance, ActiveClient, DeviceStatus, MaintenanceAction, MaintenanceCoordinator,
    MaintenanceRequest, MessageLogEntry,
};
use whatsapp_esp32::{
    crash, metrics, Esp32Executor, Esp32Runtime, Esp32TransportFactory, EspHttpClient, NvsStore,
};

// The whole Rust heap goes to PSRAM; internal DRAM is left to FreeRTOS, DMA
// and mbedTLS. The library only defines the allocator, the firmware installs it.
//
// Only where the build has PSRAM. On the ESP32-C3 there is one heap and it is
// internal DRAM, so `PsramAllocator` would ask `heap_caps_aligned_alloc` for
// SPIRAM, get a null, and retry against the default heap on every single
// allocation. Leaving it out means the plain ESP-IDF allocator, which is what
// that fallback path reaches anyway.
#[cfg(esp_idf_spiram)]
#[global_allocator]
static ALLOCATOR: whatsapp_esp32::psram_alloc::PsramAllocator =
    whatsapp_esp32::psram_alloc::PsramAllocator;

// WiFi + server come from .env at build time (see .env.example). option_env! keeps
// a fresh clone with no .env compiling; the firmware reports missing WiFi at runtime.
#[cfg(not(feature = "qemu"))]
const WIFI_SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
#[cfg(not(feature = "qemu"))]
const WIFI_PASS: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

// WebSocket URL of the mock server or WhatsApp gateway. Override via WHATSAPP_WS_URL in .env.
// Under the `qemu` feature the default is the emulator's view of the host: QEMU's user-mode
// network hands the guest 10.0.2.15 and exposes the host as 10.0.2.2, so a mock server
// listening on the host's port 8080 is reachable without any forwarding rule. Without
// `mock-server` the default is the real gateway, matching the verification that build does.
const MOCK_SERVER_WS: &str = match option_env!("WHATSAPP_WS_URL") {
    Some(u) => u,
    None if cfg!(feature = "qemu") => "wss://10.0.2.2:8080/ws/chat",
    None if cfg!(feature = "mock-server") => "wss://192.168.0.4:8080/ws/chat",
    None => whatsapp_rust::wacore::net::WHATSAPP_WEB_WS_URL,
};
// Accept whatever certificate the server presents. The mock server mints a fresh
// self-signed one on every start; the real gateway must be verified.
const SKIP_TLS_VERIFY: bool = cfg!(feature = "mock-server");

// DEV-ONLY auto-pair: when the bot emits a pairing QR, POST it to the bartender mock
// server's `/admin/mock-phone/scan-qr` endpoint, which completes pairing as if a phone
// scanned it (mirrors whatsapp-rust e2e `spawn_qr_autoresponder_http`). Set false for the
// real WhatsApp gateway (no such endpoint exists there; you scan with your phone).
const MOCK_AUTOPAIR: bool = cfg!(feature = "mock-server");

// The name this device pairs under. Against the mock server the push name is also
// what selects the account, so two boards with the same name share one number.
// `WHATSAPP_PUSH_NAME` in .env bakes one in; the `push_name` key of the `wa`
// namespace in the default NVS partition overrides it at flash time (see README
// "Configure"), which is how the two-board QEMU test tells its boards apart from
// one firmware image.
const DEFAULT_PUSH_NAME: &str = match option_env!("WHATSAPP_PUSH_NAME") {
    Some(n) => n,
    None => "esp32-test",
};
/// Shared secret the sensitive admin routes require, when one is configured.
/// Same two sources as the push name: `.env` at build time, or the `wa`
/// namespace of the default NVS partition (key `admin_token`) at flash time.
/// Empty means unset, which keeps the historical unauthenticated behavior.
#[cfg(feature = "admin")]
const DEFAULT_ADMIN_TOKEN: &str = match option_env!("ADMIN_TOKEN") {
    Some(t) => t,
    None => "",
};

/// What Linked Devices shows as the device's OS.
const DEVICE_OS: &str = "whatsapp-esp32";

/// Where the dashboard answers: `http://<MDNS_HOSTNAME>.local:<ADMIN_PORT>/dashboard`.
#[cfg(feature = "admin")]
const MDNS_HOSTNAME: &str = "esp32-whatsapp";
#[cfg(feature = "admin")]
const ADMIN_PORT: u16 = admin::DEFAULT_ADMIN_PORT;

const PING_TRIGGER: &str = "\u{1f980}ping"; // 🦀ping
const PONG_TEXT: &str = "\u{1f3d3} Pong!"; // 🏓 Pong!
const REACTION_EMOJI: &str = "\u{1f3d3}"; // 🏓

/// Why the last client instance stopped, and what the supervisor does about it.
/// Ordered by severity so concurrent events keep the strongest outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ClientExit {
    /// Rebuild the client after a short delay (the default for any exit).
    Restart = 0,
    /// The server said when to come back; wait that long first.
    TemporaryBan = 1,
    /// The server rejected the client in a way a retry cannot fix (outdated
    /// build, stream replaced, generic failure), but the credentials are still
    /// valid. Stop retrying rather than hammer the server; a reboot retries.
    PreserveCredentials = 2,
    /// The server unlinked this device. The stored credentials are dead and
    /// must be erased so the next boot pairs afresh instead of looping on 401.
    ResetCredentials = 3,
}

impl ClientExit {
    fn from_u8(value: u8) -> Self {
        match value {
            3 => Self::ResetCredentials,
            2 => Self::PreserveCredentials,
            1 => Self::TemporaryBan,
            _ => Self::Restart,
        }
    }
}

/// Watches the lifecycle events that decide how a client instance ends. A
/// registered handler rather than the `on_event` closure because it must see
/// the event even when the closure's own future is behind.
struct ClientExitObserver {
    outcome: Arc<AtomicU8>,
    temporary_ban_seconds: Arc<AtomicU32>,
}

impl EventHandler for ClientExitObserver {
    fn handle_event(&self, event: Arc<Event>) {
        let outcome = match &*event {
            // A `<failure>`/`<stream:error>` from the server with the logged-out
            // reason: the phone removed this device. Our own `logout()` also
            // dispatches LoggedOut, without a raw stanza, and is handled by the
            // maintenance task that called it.
            Event::LoggedOut(logout)
                if logout.raw.is_some()
                    && matches!(&logout.reason, ConnectFailureReason::LoggedOut) =>
            {
                ClientExit::ResetCredentials
            }
            Event::LoggedOut(logout) if logout.raw.is_some() => ClientExit::PreserveCredentials,
            Event::TemporaryBan(ban) => {
                self.temporary_ban_seconds.store(
                    u32::try_from(ban.expire.num_seconds().max(0)).unwrap_or(u32::MAX),
                    Ordering::Release,
                );
                ClientExit::TemporaryBan
            }
            Event::ClientOutdated(_) | Event::StreamReplaced(_) => ClientExit::PreserveCredentials,
            Event::ConnectFailure(failure) if !failure.reason.should_reconnect() => {
                ClientExit::PreserveCredentials
            }
            _ => return,
        };
        self.outcome.fetch_max(outcome as u8, Ordering::Release);
    }

    fn interest(&self) -> EventInterest {
        EventInterest::of(&[
            EventKind::LoggedOut,
            EventKind::ClientOutdated,
            EventKind::StreamReplaced,
            EventKind::TemporaryBan,
            EventKind::ConnectFailure,
        ])
    }
}

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
        // Async timers are short-lived RAII objects; their DEBUG drop message is
        // routine noise rather than a resource failure.
        esp_idf_svc::sys::esp_log_level_set(
            c"esp_idf_svc::timer".as_ptr(),
            esp_idf_svc::sys::esp_log_level_t_ESP_LOG_INFO,
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

    // The credentials come up before the network: a store that cannot be opened
    // is not something to pair over, and a reboot is the only recovery that
    // does not erase it (see NvsStore::open on why it never self-repairs).
    let store = match NvsStore::open_default() {
        Ok(store) => Arc::new(store),
        Err(error) => {
            error!("Failed to open WhatsApp NVS: {error}; rebooting to retry");
            std::thread::sleep(std::time::Duration::from_secs(5));
            unsafe { esp_idf_svc::sys::esp_restart() }
        }
    };

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let push_name = match nvs_string(&nvs, "push_name") {
        Ok(Some(name)) => {
            info!("Push name from NVS: {name}");
            name
        }
        Ok(None) => DEFAULT_PUSH_NAME.to_string(),
        Err(e) => {
            warn!("Could not read 'push_name' from NVS ({e}); falling back to default");
            DEFAULT_PUSH_NAME.to_string()
        }
    };
    // Never logged: it is a shared secret, and the boot log is not private.
    // If an admin token was configured in NVS but cannot be read (corrupt NVS or
    // type error), fail startup instead of silently disabling authentication.
    #[cfg(feature = "admin")]
    let admin_token = match nvs_string(&nvs, "admin_token") {
        Ok(Some(token)) => Some(token),
        Ok(None) => Some(DEFAULT_ADMIN_TOKEN.to_string()).filter(|t| !t.is_empty()),
        Err(e) => {
            error!(
                "Configured admin_token in NVS could not be read: {e}. Failing startup to prevent unauthenticated admin access."
            );
            return Err(e);
        }
    };

    // The network handle must stay alive for the rest of main(): dropping it tears
    // the interface down. Which interface that is depends on where the firmware runs.
    #[cfg(not(feature = "qemu"))]
    let (_net, _ip) = bring_up_wifi(peripherals.modem, sysloop.clone(), nvs)?;
    #[cfg(feature = "qemu")]
    let (_net, _ip) = bring_up_ethernet(peripherals.mac, sysloop.clone(), nvs)?;

    let _sntp = esp_idf_svc::sntp::EspSntp::new_default()?;
    std::thread::sleep(std::time::Duration::from_secs(2));

    #[cfg(feature = "admin")]
    let _mdns = {
        let mut mdns = esp_idf_svc::mdns::EspMdns::take()?;
        mdns.set_hostname(MDNS_HOSTNAME)?;
        mdns.set_instance_name("ESP32 WhatsApp")?;
        mdns.add_service(None, "_http", "_tcp", ADMIN_PORT, &[("path", "/dashboard")])?;
        mdns
    };

    let device_status = Arc::new(DeviceStatus::new());
    let active_client = Arc::new(ActiveClient::new());
    let maintenance = Arc::new(MaintenanceCoordinator::new());
    // The runtime every Bot is built with, and the executor that runs them.
    let (runtime, executor) = Esp32Runtime::create_default()?;
    // The admin server spawns onto the same executor (send, pair code,
    // maintenance), through the runtime's queue so it can tell when a spawn
    // was refused.
    #[cfg(feature = "admin")]
    let _admin_server = admin::start_admin_server(
        store.clone(),
        device_status.clone(),
        active_client.clone(),
        maintenance.clone(),
        runtime.spawner(),
        Arc::new(admin::AdminAuth::new(admin_token)),
        ADMIN_PORT,
    )?;
    #[cfg(feature = "admin")]
    info!("Admin: http://{MDNS_HOSTNAME}.local:{ADMIN_PORT}/dashboard");
    #[cfg(feature = "admin")]
    info!("Admin: http://{_ip}:{ADMIN_PORT}/dashboard");

    info!(
        "Free heap: {} bytes (internal: {} bytes)",
        unsafe { esp_idf_svc::sys::esp_get_free_heap_size() },
        unsafe { esp_idf_svc::sys::esp_get_free_internal_heap_size() }
    );

    let jh = spawn_thread(&Esp32Executor::default_thread_config(), move || {
        run_executor(
            executor,
            runtime,
            store,
            device_status,
            active_client,
            maintenance,
            push_name,
        );
    })?;

    if let Err(e) = jh.join() {
        error!("Executor thread panicked: {:?}", e);
    }
    Ok(())
}

/// A string provisioned in the default NVS partition, namespace `wa`.
///
/// Provisioning is a plain NVS record written with `nvs_partition_gen` at flash
/// time or by any tool that can write that partition, which is how one firmware
/// image can be flashed to several boards that differ only in these values.
fn nvs_string(nvs: &EspDefaultNvsPartition, key: &str) -> Result<Option<String>> {
    // A read-only open of a namespace that was never written fails with
    // NVS_NOT_FOUND. That is the unprovisioned case, not an error.
    let namespace = match EspNvs::new(nvs.clone(), "wa", false) {
        Ok(ns) => ns,
        Err(e) if e.code() == esp_idf_svc::sys::ESP_ERR_NVS_NOT_FOUND => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("Failed to open NVS namespace 'wa': {e}")),
    };
    let mut buf = [0u8; 512];
    match namespace.get_str(key, &mut buf) {
        Ok(Some(value)) if !value.is_empty() => Ok(Some(value.to_string())),
        Ok(_) => Ok(None),
        Err(e) if e.code() == esp_idf_svc::sys::ESP_ERR_NVS_NOT_FOUND => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Failed to read '{key}' from NVS: {e}")),
    }
}

/// Station-mode WiFi: the real board. Blocks until the interface has an address.
///
/// Returns the handle the caller must keep alive, plus the address for the log.
#[cfg(not(feature = "qemu"))]
fn bring_up_wifi(
    modem: esp_idf_svc::hal::modem::Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<(BlockingWifi<EspWifi<'static>>, std::net::Ipv4Addr)> {
    // WIFI_SSID is injected at build time via option_env!; it is legitimately empty
    // in a clone-and-run build with no .env. `black_box` hides the constant from the
    // optimizer: without it, an empty SSID makes this `return` provably always
    // taken, and LTO then drops everything after it, the whole WhatsApp client
    // included, leaving a 260 KB stub image that says nothing about whether the
    // real firmware still builds or fits.
    if std::hint::black_box(WIFI_SSID).is_empty() {
        error!(
            "WiFi is not configured. Copy .env.example to .env, set WIFI_SSID / WIFI_PASS, and reflash."
        );
        return Err(anyhow::anyhow!("WIFI_SSID is empty"));
    }

    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop)?;

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

    let ip = wifi.wifi().sta_netif().get_ip_info()?.ip;
    info!("WiFi connected! IP: {ip}");
    Ok((wifi, ip))
}

/// OpenCores Ethernet: the interface Espressif's QEMU attaches to the emulated
/// ESP32-S3 (`-nic user,model=open_eth`). There is no radio to emulate, so this is
/// the only way the firmware gets on the network under QEMU; it needs
/// `CONFIG_ETH_USE_OPENETH=y` from `sdkconfig.qemu`, which is also what makes
/// `peripherals.mac` exist on this chip. Blocks until DHCP has handed out an address.
///
/// The NVS handle is unused here (WiFi keeps its calibration data in NVS, Ethernet
/// has none), but taking it keeps the two bring-ups the same shape for `main`.
#[cfg(feature = "qemu")]
fn bring_up_ethernet(
    mac: esp_idf_svc::hal::mac::MAC<'static>,
    sysloop: EspSystemEventLoop,
    _nvs: EspDefaultNvsPartition,
) -> Result<(
    esp_idf_svc::eth::BlockingEth<esp_idf_svc::eth::EspEth<'static, esp_idf_svc::eth::OpenEth>>,
    std::net::Ipv4Addr,
)> {
    use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver};

    let driver = EthDriver::new_openeth(mac, sysloop.clone())?;
    let mut eth = BlockingEth::wrap(EspEth::wrap(driver)?, sysloop)?;
    eth.start()?;
    info!("Ethernet (OpenCores/QEMU) started, waiting for DHCP...");
    eth.wait_netif_up()?;
    let ip = eth.eth().netif().get_ip_info()?.ip;
    info!("Ethernet connected! IP: {ip}");
    Ok((eth, ip))
}

/// The `wa-main` thread body: the executor, with the supervisor as its main
/// future. `block_on` returns only if the supervisor does, which it never does.
fn run_executor(
    executor: Esp32Executor,
    runtime: Esp32Runtime,
    store: Arc<NvsStore>,
    device_status: Arc<DeviceStatus>,
    active_client: Arc<ActiveClient>,
    maintenance: Arc<MaintenanceCoordinator>,
    push_name: String,
) {
    // Register this thread with the task watchdog: with the idle-task check off
    // for this core (sdkconfig.defaults), a wedged event loop is otherwise
    // invisible. The feed runs as a task, so it stops exactly when the loop does.
    let watchdog_registered = unsafe {
        esp_idf_svc::sys::esp_task_wdt_add(core::ptr::null_mut()) == esp_idf_svc::sys::ESP_OK
    };
    if watchdog_registered {
        runtime.spawn(Box::pin(feed_task_watchdog())).detach();
    } else {
        error!("Failed to register wa-main with the task watchdog");
    }

    // The zlib inflate state (one ~47.5 KB block) is built on this thread the
    // first time a compressed frame is decoded, which on a board without PSRAM
    // is the moment the login's largest frame is already sitting in the heap:
    // the ESP32-C3 has ~58 KB free then, so the block never fits next to it.
    // Built here, on a fresh heap, it is parked in the thread's pool and reused
    // by every compressed frame this executor ever decodes.
    whatsapp_rust::wacore_binary::zlib_pool::warm_pool();

    executor.block_on(run_whatsapp(
        runtime,
        store,
        device_status,
        active_client,
        maintenance,
        push_name,
    ));
}

/// Feeds the task watchdog `wa-main` registered itself with, at a fraction of
/// the configured timeout so ordinary scheduling jitter has room.
///
/// Every error here is transient by nature (the async timer is an allocation),
/// so none of them may end the loop: a feeder that returns leaves the watchdog
/// armed with nobody feeding it, which reboots a device that is working fine.
/// Only a run of failures long enough to be a real fault stops it, and then it
/// unregisters first so the reboot it can no longer prevent never happens.
async fn feed_task_watchdog() {
    /// Well inside `CONFIG_ESP_TASK_WDT_TIMEOUT_S` (30 s in sdkconfig.defaults).
    const FEED_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    /// Delay between retries when timer creation or wait fails.
    const ERROR_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
    /// About a minute of consecutive failures before giving the watchdog up.
    const MAX_CONSECUTIVE_FAILURES: u32 = 12;

    let mut timer_service = esp_idf_svc::timer::EspTaskTimerService::new().ok();
    let mut timer = timer_service.as_ref().and_then(|s| s.timer_async().ok());
    let mut failures = 0u32;
    loop {
        // Feed first: the point of this task is that the watchdog gets fed even
        // on an iteration where arming the next timer went wrong.
        let result = unsafe { esp_idf_svc::sys::esp_task_wdt_reset() };
        if result != esp_idf_svc::sys::ESP_OK {
            failures += 1;
            error!("Failed to feed the wa-main task watchdog: ESP error {result}");
        }

        // Re-arm or allocate timer if needed, retrying timer service creation if it failed
        if timer.is_none() {
            if timer_service.is_none() {
                timer_service = esp_idf_svc::timer::EspTaskTimerService::new().ok();
            }
            if let Some(ref s) = timer_service {
                timer = s.timer_async().ok();
            }
        }

        let feed_cycle_ok = if let Some(ref mut t) = timer {
            match t.after(FEED_INTERVAL).await {
                Ok(()) => true,
                Err(error) => {
                    failures += 1;
                    error!("Task-watchdog feed timer failed: {error}");
                    false
                }
            }
        } else {
            failures += 1;
            error!("Could not arm the task-watchdog feed timer");
            false
        };

        if feed_cycle_ok && result == esp_idf_svc::sys::ESP_OK {
            failures = 0;
            continue;
        }

        if failures >= MAX_CONSECUTIVE_FAILURES {
            // Unregister before leaving. A watchdog nobody feeds is a guaranteed
            // reboot loop, which hides the fault instead of reporting it.
            error!(
                "Giving up on the task watchdog after {failures} consecutive failures; unregistering wa-main"
            );
            unsafe { esp_idf_svc::sys::esp_task_wdt_delete(core::ptr::null_mut()) };
            return;
        }
        // The timer is unusable this round; back off briefly and yield to executor
        // so transient timer failures do not spin or permanently starve wa-main.
        std::thread::sleep(ERROR_RETRY_DELAY);
        futures::future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            std::task::Poll::Ready(())
        })
        .await;
    }
}

async fn run_whatsapp(
    runtime: Esp32Runtime,
    store: Arc<NvsStore>,
    device_status: Arc<DeviceStatus>,
    active_client: Arc<ActiveClient>,
    maintenance: Arc<MaintenanceCoordinator>,
    push_name: String,
) {
    // Supervisor loop: the firmware must never let app_main() return (that tears down
    // WiFi/admin and halts the chip). whatsapp-rust reconnects forever internally, but
    // if the bot future ever does end, re-create it after a short delay. This task also
    // keeps `runtime` alive, so the executor's spawn channel never closes.
    loop {
        let outcome = run_whatsapp_inner(
            runtime.clone(),
            store.clone(),
            device_status.clone(),
            active_client.clone(),
            push_name.clone(),
        )
        .await;
        let delay = match outcome {
            Ok(ClientExit::Restart) => {
                warn!("WhatsApp client exited; restarting in 5s");
                5
            }
            Ok(ClientExit::TemporaryBan) => {
                let wait = TEMPORARY_BAN_WAIT.swap(0, Ordering::AcqRel).max(1);
                warn!("WhatsApp temporarily banned this client; retrying in {wait}s");
                u64::from(wait)
            }
            Ok(ClientExit::PreserveCredentials) => {
                error!(
                    "WhatsApp rejected the client without invalidating its credentials; keeping them and stopping automatic retries (reboot to retry)"
                );
                futures::future::pending::<()>().await;
                unreachable!()
            }
            Ok(ClientExit::ResetCredentials) => {
                warn!("WhatsApp unlinked this device; erasing the stored credentials");
                // The dashboard's actions end in the same place, so go through the
                // coordinator rather than erasing here: it decides who runs the
                // work. `Start` is ours to run; `Queued` means a dashboard action
                // is already in flight and has just been upgraded to a reset, and
                // `Rejected` means the reboot is already committed. In both of
                // those the other task finishes and reboots, so this one must wait
                // rather than erase and restart a second time underneath it.
                match maintenance.request(MaintenanceAction::Reset) {
                    MaintenanceRequest::Start => {
                        run_maintenance(
                            store.clone(),
                            device_status.clone(),
                            active_client.clone(),
                            maintenance.clone(),
                        )
                        .await;
                    }
                    MaintenanceRequest::Queued | MaintenanceRequest::Rejected => {
                        info!("Maintenance is already running; it will reboot this device");
                    }
                }
                futures::future::pending::<()>().await;
                unreachable!("maintenance reboots the device")
            }
            Err(e) => {
                error!("WhatsApp client error: {e}; restarting in 5s");
                5
            }
        };
        // `Runtime::sleep` fails open (returns at once if no esp_timer could be
        // armed), so check the clock: a restart storm must not follow a timer
        // allocation failure. The fallback waits on the blocking worker, never
        // on the executor thread, so the watchdog feed keeps running meanwhile.
        let delay_dur = std::time::Duration::from_secs(delay);
        let start = std::time::Instant::now();
        while start.elapsed() < delay_dur {
            runtime
                .sleep(delay_dur.saturating_sub(start.elapsed()))
                .await;
            if start.elapsed() < delay_dur {
                runtime
                    .spawn_blocking(Box::new(|| {
                        std::thread::sleep(std::time::Duration::from_millis(250))
                    }))
                    .await;
            }
        }
    }
}

/// Seconds a temporary ban asked us to wait, handed from the client instance
/// that saw the ban to the supervisor that sleeps it off.
static TEMPORARY_BAN_WAIT: AtomicU32 = AtomicU32::new(0);

async fn run_whatsapp_inner(
    runtime: Esp32Runtime,
    backend: Arc<NvsStore>,
    device_status: Arc<DeviceStatus>,
    active_client: Arc<ActiveClient>,
    push_name: String,
) -> Result<ClientExit> {
    // Both would be `::default()` against the real gateway; the demo points them
    // at the mock server and accepts its self-signed certificate.
    let transport_factory = Esp32TransportFactory::new(MOCK_SERVER_WS, SKIP_TLS_VERIFY);
    let http_client = EspHttpClient::new(SKIP_TLS_VERIFY);

    // One-shot guard so dev auto-pair POSTs the QR only once per bot session.
    let scanned = Arc::new(AtomicBool::new(false));
    let exit_outcome = Arc::new(AtomicU8::new(ClientExit::Restart as u8));
    let temporary_ban_seconds = Arc::new(AtomicU32::new(0));

    let event_status = device_status.clone();
    let event_active_client = active_client.clone();
    let bot = Bot::builder()
        .with_backend_arc(backend)
        .with_transport_factory(transport_factory)
        .with_http_client(http_client)
        .with_runtime(runtime)
        .with_push_name(push_name)
        .with_version((2, 3000, 0))
        .with_device_props(
            DevicePropsOverride::new()
                .with_os(DEVICE_OS)
                .with_platform_type(wa::device_props::PlatformType::CHROME),
        )
        // 20 instead of the 812 default: generating 812 X25519 keypairs at login
        // exhausts internal DRAM, and even on the blocking worker it is seconds
        // of work the first connect would wait on. Configurable upstream as of
        // whatsapp-rust PR #695, so no fork needed.
        //
        // 50 was the first choice and it was still too many for the ESP32-C3, for
        // a reason that is about timing rather than size: the upload's success
        // marks the device dirty, and the persistence save then runs on `wa-nvs`
        // at the same instant the WebSocket thread reserves for the inbound
        // AB-props frame. That reserve wants 32,300 contiguous bytes and had
        // 51,200 a frame earlier, so the two together are what abort the chip.
        // Fewer keys shrinks the upload node (2,717 bytes at 50) and the burst
        // of small allocations that stores them, which is what fragments the
        // heap ahead of that reserve. It does not shrink the device record:
        // `wacore::store::Device` holds keys and identity, never the prekey
        // pool, which lives in the backend. The floor upstream is 5; 20 keeps
        // four times that, and the client re-uploads when the server runs low.
        .with_wanted_pre_key_count(20)
        // The AB-props catalog is experiment flags, and its response is by far
        // the largest frame this firmware ever receives: 28,204 bytes, against
        // a largest free block that the concurrent background init has eroded
        // to ~28.6 KB by the time it lands. Upstream now streams the inflate
        // rather than buffering it whole, which removed the decompression wall
        // -- but the frame itself still has to be materialised contiguously,
        // and on the C3 it misses by a few hundred bytes.
        //
        // So the boards that cannot hold the answer do not ask the question.
        // Turning the fetch off is a supported client option, not a workaround:
        // the query is simply not sent, the flags stay at their registry
        // defaults, and nothing else about the connection changes. PSRAM boards
        // keep the default they were tested with.
        .with_ab_props_fetch(crate::runtime::HAS_PSRAM)
        // Message history is not persisted (only identity and Signal state are),
        // so a history sync buys this firmware nothing and costs it a lot:
        // upstream measures the drain at ~14 MB of allocation churn, more than
        // the whole PSRAM. Drop this line if the store ever keeps the history.
        .skip_history_sync()
        .with_event_handler(ClientExitObserver {
            outcome: exit_outcome.clone(),
            temporary_ban_seconds: temporary_ban_seconds.clone(),
        })
        .on_event(move |event, client| {
            let ds = event_status.clone();
            let scanned = scanned.clone();
            let active_client = event_active_client.clone();
            async move {
                // A superseded instance can still drain a few events while the
                // new one starts; they must not touch the shared status.
                if !active_client.is_current(&client) {
                    return;
                }
                match &*event {
                    // 0.7.0 sealed the event payloads into `#[non_exhaustive]`
                    // structs, so this is a tuple variant now, not `{ code, timeout }`.
                    Event::PairingQrCode(qr) => {
                        info!("QR CODE (valid for {}s)", qr.timeout.as_secs());
                        ds.set_qr_code(qr.code.clone());
                        if MOCK_AUTOPAIR && !scanned.swap(true, Ordering::Relaxed) {
                            auto_scan_qr(&qr.code).await;
                        }
                    }
                    Event::PairingQrCodesExhausted(_) => {
                        warn!("Pairing QR codes exhausted; reconnect to get a new one");
                        ds.clear_qr_code();
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

                        let pn = client.pn().map(|j| j.to_string());
                        let lid = client.lid().map(|j| j.to_string());
                        ds.set_connected(pn, lid);
                        info!("Connected to WhatsApp!");
                        info!("Free heap: {} bytes", unsafe {
                            esp_idf_svc::sys::esp_get_free_heap_size()
                        });
                    }
                    // The dashboard used to keep showing "Connected" forever after
                    // the socket dropped, because nothing ever cleared the flag.
                    Event::Disconnected(d) => {
                        warn!("Disconnected: {}", d.reason);
                        ds.set_disconnected();
                    }
                    Event::LoggedOut(l) => {
                        info!("Logged out: {:?}", l.reason);
                        ds.set_logged_out();
                    }
                    // `Event::Message(msg, info)` became `Event::Messages(batch)`:
                    // an offline drain now delivers one batch per durable commit
                    // instead of one event per message, so this iterates.
                    Event::Messages(batch) => {
                        for inbound in batch {
                            let text = inbound.message.text_content();
                            info!(
                                "Message from {}: {:?}",
                                inbound.info.source.sender,
                                text.unwrap_or("<no text>")
                            );
                            ds.record_message(MessageLogEntry {
                                id: inbound.info.id.to_string(),
                                chat: inbound.info.source.chat.to_string(),
                                sender: inbound.info.source.sender.to_string(),
                                text: text.map(str::to_owned),
                                timestamp: inbound.info.timestamp.timestamp(),
                                from_me: inbound.info.source.is_from_me,
                            });
                            if text == Some(PING_TRIGGER) {
                                let ctx =
                                    MessageContext::from_inbound(inbound, Arc::clone(&client));
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
                        // These fire either side of the background init burst
                        // (Props, Blocklist, Privacy, Digest, Devices go out
                        // together via `futures::join!`), which is where the C3
                        // loses ~54 KB between connect and the props response.
                        // A per-task profile here says which stack or which
                        // pool is holding it, instead of leaving it inferred.
                        crate::metrics::log_memory_profile("event");
                    }
                }
            }
        })
        .build()
        .await?;

    let client = bot.client();
    active_client.set(client.clone());
    info!("Bot built, starting run loop...");
    bot.run().await;
    active_client.clear_if(&client);

    let outcome = ClientExit::from_u8(exit_outcome.load(Ordering::Acquire));
    match outcome {
        ClientExit::ResetCredentials => device_status.set_logged_out(),
        ClientExit::Restart | ClientExit::TemporaryBan | ClientExit::PreserveCredentials => {
            device_status.set_disconnected()
        }
    }
    TEMPORARY_BAN_WAIT.store(
        temporary_ban_seconds
            .load(Ordering::Acquire)
            .saturating_add(5),
        Ordering::Release,
    );
    Ok(outcome)
}

/// DEV-ONLY: POST the pairing QR to the bartender mock server's scan-qr admin
/// endpoint, completing pairing without a phone. Derives the admin URL from
/// `MOCK_SERVER_WS` (ws→http / wss→https, path `/admin/mock-phone/scan-qr`),
/// matching whatsapp-rust's e2e helper. The request is blocking HTTP done inline;
/// it briefly stalls the executor once, which is fine for a one-shot pairing step.
async fn auto_scan_qr(code: &str) {
    let (host, port, _path, tls) = match whatsapp_esp32::http_client::parse_url(MOCK_SERVER_WS) {
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
        body: Some(bytes::Bytes::copy_from_slice(code.as_bytes())),
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
    // `ctx.react` builds the referential MessageKey itself (including the
    // group/status participant, which the hand-rolled key here used to get wrong
    // for status broadcasts) and allocates no JID strings on this path.
    info!("Sending reaction...");
    if let Err(e) = ctx.react(REACTION_EMOJI).await {
        error!("Failed to send reaction: {}", e);
    }
    info!("Reaction sent! Building pong reply...");

    let start = std::time::Instant::now();
    // Equivalent to the old build_quote_context + ExtendedTextMessage literal.
    let sent = match ctx.reply_quoting(PONG_TEXT).await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to send pong: {}", e);
            return;
        }
    };

    let duration = format!("{:.2?}", start.elapsed());
    info!(
        "Send took {}. Editing message {}...",
        duration, sent.message_id
    );

    // buffa (the prost replacement in 0.7.0) wraps sub-messages in
    // `MessageField<T>` instead of `Option<Box<T>>`.
    let edit = wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some(format!("{PONG_TEXT}\n`{duration}`")),
            ..Default::default()
        }),
        ..Default::default()
    };
    if let Err(e) = ctx.edit_message(sent.message_id.clone(), edit).await {
        error!("Failed to edit message {}: {}", sent.message_id, e);
    }
}
