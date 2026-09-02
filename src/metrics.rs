//! Live ESP32 system telemetry for the admin dashboard. Every value is read
//! straight from ESP-IDF accessors, nothing here is computed or guessed.

use std::sync::OnceLock;

use esp_idf_svc::hal::reset::ResetReason;
use whatsapp_rust::serde_json;
use esp_idf_svc::sys;

/// The reset reason captured once at boot (why the PREVIOUS run ended).
static LAST_RESET: OnceLock<ResetReason> = OnceLock::new();
/// The Rust panic message from the run that crashed, if the previous reset was a
/// panic and the RTC buffer survived (warm reboot). `None` for a hardware
/// exception (no Rust string) or any non-panic reset.
static LAST_PANIC: OnceLock<Option<String>> = OnceLock::new();

pub fn set_last_reset(r: ResetReason) {
    let _ = LAST_RESET.set(r);
}

pub fn set_last_panic(msg: Option<String>) {
    let _ = LAST_PANIC.set(msg);
}

/// Core dump summary from the previous crash (panic or hw exception), if any.
static LAST_COREDUMP: OnceLock<Option<crate::crash::CoredumpSummary>> = OnceLock::new();

pub fn set_last_coredump(c: Option<crate::crash::CoredumpSummary>) {
    let _ = LAST_COREDUMP.set(c);
}

fn coredump_json() -> Option<serde_json::Value> {
    LAST_COREDUMP.get().and_then(|o| o.as_ref()).map(|c| {
        serde_json::json!({
            "task": c.task,
            "exc_pc": format!("0x{:08x}", c.exc_pc),
            "exc_cause": c.exc_cause,
            "fault_addr": format!("0x{:08x}", c.fault_addr),
            "backtrace": c.backtrace.iter().map(|p| format!("0x{p:08x}")).collect::<Vec<_>>(),
            "bt_corrupted": c.bt_corrupted,
        })
    })
}

fn last_panic() -> Option<&'static str> {
    LAST_PANIC.get().and_then(|o| o.as_deref())
}

/// One-line suffix for the boot log, e.g. ` | panic: panicked at src/x.rs:5: ...`.
pub fn last_panic_str() -> String {
    match last_panic() {
        Some(m) => format!(" | panic: {m}"),
        None => String::new(),
    }
}

fn last_reset_str() -> &'static str {
    match LAST_RESET.get().copied() {
        Some(ResetReason::PowerOn) => "PowerOn",
        Some(ResetReason::Software) => "Software (esp_restart)",
        Some(ResetReason::Panic) => "Panic / exception (crash)",
        Some(ResetReason::TaskWatchdog) => "TaskWatchdog",
        Some(ResetReason::InterruptWatchdog) => "InterruptWatchdog",
        Some(ResetReason::Watchdog) => "Watchdog",
        Some(ResetReason::Brownout) => "Brownout (power dip)",
        Some(ResetReason::DeepSleep) => "DeepSleep",
        Some(ResetReason::ExternalPin) => "ExternalPin",
        Some(ResetReason::JTAG) => "JTAG",
        Some(ResetReason::USBPeripheral) => "USBPeripheral",
        Some(_) => "Other",
        None => "Unknown",
    }
}

/// Minimum free stack ever (bytes) for a FreeRTOS task by name. `StackType_t`
/// is `u8` on this port, so the high-water mark is already in bytes. `None` if
/// the task isn't registered (e.g. before it spawns).
fn stack_free_min(name: &core::ffi::CStr) -> Option<u32> {
    let handle = unsafe { sys::xTaskGetHandle(name.as_ptr()) };
    if handle.is_null() {
        None
    } else {
        Some(unsafe { sys::uxTaskGetStackHighWaterMark(handle) } as u32)
    }
}

/// Snapshot of heap / DRAM / PSRAM / uptime / RSSI as JSON for `/metrics`.
pub fn system_metrics_json() -> String {
    // SAFETY: all are read-only ESP-IDF accessors, safe to call from any context.
    let heap_free = unsafe { sys::esp_get_free_heap_size() };
    let heap_internal_free = unsafe { sys::esp_get_free_internal_heap_size() };
    let heap_min_free = unsafe { sys::esp_get_minimum_free_heap_size() };
    let internal_largest_block =
        unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL) };
    let internal_min_free =
        unsafe { sys::heap_caps_get_minimum_free_size(sys::MALLOC_CAP_INTERNAL) };
    // The exact pool a FreeRTOS mutex/semaphore allocates from (the one whose
    // exhaustion produced the `xSemaphoreCreateMutex -> EAGAIN` crash). Watch this,
    // not the bare-INTERNAL number, to judge real headroom.
    let cap_8bit = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    let internal_8bit_largest = unsafe { sys::heap_caps_get_largest_free_block(cap_8bit) };
    let internal_8bit_min_free = unsafe { sys::heap_caps_get_minimum_free_size(cap_8bit) };
    let psram_free = unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM) };
    let psram_largest_block =
        unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_SPIRAM) };
    let uptime_s = unsafe { sys::esp_timer_get_time() } / 1_000_000;

    // Right-sizing data: how close each task came to overflowing its stack.
    let stack_wa_main = stack_free_min(c"wa-main");
    let stack_ws_transport = stack_free_min(c"ws-transport");

    // No radio under QEMU (the network is the emulated Ethernet MAC), so the
    // WiFi driver is never initialized there and this query has nothing to ask.
    #[cfg(feature = "qemu")]
    let rssi: Option<core::ffi::c_int> = None;
    #[cfg(not(feature = "qemu"))]
    let rssi = {
        let mut rssi: core::ffi::c_int = 0;
        if unsafe { sys::esp_wifi_sta_get_rssi(&mut rssi) } == sys::ESP_OK {
            Some(rssi)
        } else {
            None
        }
    };

    serde_json::json!({
        "reset_reason": last_reset_str(),
        "uptime_s": uptime_s,
        "heap_free": heap_free,
        "heap_internal_free": heap_internal_free,
        "heap_min_free": heap_min_free,
        "internal_largest_block": internal_largest_block,
        "internal_min_free": internal_min_free,
        "internal_8bit_largest_block": internal_8bit_largest,
        "internal_8bit_min_free": internal_8bit_min_free,
        "psram_free": psram_free,
        "psram_largest_block": psram_largest_block,
        "stack_wa_main_min": stack_wa_main,
        "stack_ws_transport_min": stack_ws_transport,
        "rssi_dbm": rssi,
        "last_panic": last_panic(),
        "coredump": coredump_json(),
    })
    .to_string()
}
