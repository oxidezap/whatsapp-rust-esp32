//! Live ESP32 system telemetry for the admin dashboard. Every value is read
//! straight from ESP-IDF accessors, nothing here is computed or guessed.

use std::sync::OnceLock;

use esp_idf_svc::hal::reset::ResetReason;
use esp_idf_svc::sys;
use whatsapp_rust::serde_json;

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
    let mut handle = unsafe { sys::xTaskGetHandle(name.as_ptr()) };
    if handle.is_null() && name == c"wa-main" {
        handle = unsafe { sys::xTaskGetHandle(c"main".as_ptr()) };
    }
    if handle.is_null() {
        None
    } else {
        Some(unsafe { sys::uxTaskGetStackHighWaterMark(handle) } as u32)
    }
}

/// `free/largest` for the internal heap, as a short string for an existing log line.
///
/// The per-frame logs in `transport` carry this on a board without PSRAM. A
/// once-per-connect snapshot was enough to show that the ESP32-C3 arrives at its
/// first connect with far less heap than the boot-time total suggests, but not
/// to show *where* it goes: between one connect and the abort the heap fell from
/// 101,356 free / 63,488 largest to less than the 32,300 bytes the next
/// allocation asked for, in 320 ms and a handful of frames. Attaching it to the
/// frames themselves is what makes that interval readable, and it costs two
/// accessor calls on a line that was already being formatted.
///
/// Empty where there is PSRAM: those boards have an 8 MB heap and the numbers
/// are noise on every frame.
pub fn heap_note() -> alloc_note::Note {
    alloc_note::Note::new()
}

/// Wrapper so the `Display` impl can decide, at no cost on a PSRAM board,
/// whether to read the heap at all.
pub mod alloc_note {
    use super::sys;

    /// Renders as ` heap=<free>/<largest>` without PSRAM, and as nothing with it.
    pub struct Note(());

    impl Note {
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            Self(())
        }
    }

    impl core::fmt::Display for Note {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            if crate::runtime::HAS_PSRAM {
                return Ok(());
            }
            let cap = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
            // SAFETY: read-only ESP-IDF accessors, safe from any context.
            let free = unsafe { sys::heap_caps_get_free_size(cap) };
            let largest = unsafe { sys::heap_caps_get_largest_free_block(cap) };
            write!(f, " heap={free}/{largest}")
        }
    }
}

/// Log every worker stack's high-water mark together with the internal heap.
///
/// The worker stacks are `by_ram` constants in `runtime`, `transport` and
/// `storage`: 14 + 6 + 4 KB of spawned threads without PSRAM, plus the 36 KB
/// IDF main task the demo firmware runs the executor on instead of spawning
/// the 32 KB `wa-main` thread. `/metrics` has reported the same high-water
/// marks all along, but the QEMU end-to-end run never reads it, so the one
/// build where the sizing actually bites is the one where nobody was looking.
///
/// It bites concretely. The ESP32-C3 reaches its first WebSocket connect with
/// 53,332 bytes free and a 31,744-byte largest block, and whether a stack is
/// twice the size it needs is the difference between a 16 KB TLS record fitting
/// and the Ethernet driver failing to allocate receive buffers.
///
/// Free bytes *and* largest block, because the two diverge under fragmentation
/// and it is the second that decides whether a large allocation succeeds.
/// Free bytes and largest contiguous block, for callers that want one short
/// line rather than the full profile above.
///
/// `log_memory_profile` prints the four stack watermarks too, which is the
/// right thing at an event boundary and the wrong thing inside a loop: the
/// crash context is a fixed 60-line window ending at the abort, and a probe
/// that prints four lines per call pushes the evidence out of it. That is not
/// hypothetical -- an earlier `memory_report()` probe in this investigation
/// answered its question and displaced the run-up to the crash doing it.
pub fn heap_now() -> (usize, usize) {
    // SAFETY: read-only ESP-IDF accessors, safe from any context.
    let cap = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    unsafe {
        (
            sys::heap_caps_get_free_size(cap),
            sys::heap_caps_get_largest_free_block(cap),
        )
    }
}

/// TEMPORARY DIAGNOSTIC (PR #15 C3 triage): walk every internal heap's block
/// headers with pure arithmetic -- read each size word, compute the next
/// header, range-check it -- and report the first insane header instead of
/// faulting inside it like the IDF walks do. Returns false when a writer has
/// already corrupted the heap, so callers skip the TLSF walk and the
/// task-name lookup. Reads mapped RAM only, never dereferences a heap value,
/// so this cannot fault even on a corrupted heap. Revert before merge.
///
/// Layout constants derived empirically from `heap_caps_dump_all` output on
/// this chip: 4-byte headers (`next = cur + 4 + (raw & !3)` held for all 914
/// transitions, all sizes 4-aligned), per-heap first-block offsets below.
/// `heap_walk_selftest` (called once at boot, when the heap is healthy)
/// proves the constants before any report is trusted.
#[derive(Clone, Copy)]
struct HeapRegion {
    base: usize,
    len: usize,
    first_offset: usize,
}

/// C3 internal DRAM heaps (bases/lens from the boot `heap_init` lines).
/// `first_offset` points at the first block HEADER: the dump prints block
/// *data* addresses (`multi_heap_get_block_address`), 4 past the header, so
/// the dump-derived offsets are adjusted down by 4 here.
const C3_HEAPS: &[HeapRegion] = &[
    HeapRegion { base: 0x3FC8_FCC0, len: 0x30340, first_offset: 0x3AC },
    HeapRegion { base: 0x3FCC_0000, len: 0x1C710, first_offset: 0x368 },
    HeapRegion { base: 0x3FCD_C710, len: 0x2950, first_offset: 0x1A0 },
];

static WALK_TRUSTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

fn walk_heap(region: HeapRegion, report: bool, at: &str) -> bool {
    let end = region.base + region.len;
    let mut cur = region.base + region.first_offset;
    let mut count = 0usize;
    loop {
        if count > 100_000 {
            if report {
                log::error!("heap walk at {at}: no termination in heap {:#x}", region.base);
            }
            return false;
        }
        count += 1;
        if cur + 4 > end {
            if report {
                log::error!(
                    "heap walk at {at}: header {:#x} outside heap {:#x} (slot!)",
                    cur, region.base
                );
            }
            return false;
        }
        // SAFETY: `cur` is range-checked against mapped DRAM above; the read
        // itself cannot fault. The value is only used in arithmetic below.
        let raw = unsafe { (cur as *const u32).read_volatile() } as usize;
        let next = cur.wrapping_add(4).wrapping_add(raw & !3);
        if next <= cur || next > end {
            if report {
                // SAFETY: bounded volatile reads of mapped DRAM near `cur`;
                // shows whether the 0xa5 fill spans KBs (task-stack creation
                // overlapping live heap) or a few bytes (stray small write).
                let mut context = [0u32; 40];
                let base = cur.saturating_sub(32);
                for (i, slot) in context.iter_mut().enumerate() {
                    let addr = base + i * 4;
                    *slot = if addr + 4 <= end {
                        unsafe { (addr as *const u32).read_volatile() }
                    } else {
                        0xDEAD_DEAD
                    };
                }
                log::error!(
                    "heap walk at {at}: corrupt header at {:#x} (raw {:#x}, next {:#x}, heap {:#x})",
                    cur, raw, next, region.base
                );
                log::error!("heap walk context {:#x}: {:08x?}", base, context);
            }
            return false;
        }
        if next == end {
            return true;
        }
        cur = next;
    }
}

/// Boot-time self-test: the constants above must replay every heap exactly.
/// Call once when the heap is known healthy; reports are trusted only after.
pub fn heap_walk_selftest() {
    if crate::runtime::HAS_PSRAM {
        return;
    }
    for region in C3_HEAPS {
        if !walk_heap(*region, false, "selftest") {
            log::error!(
                "heap walk self-test FAILED on heap {:#x}: constants wrong, walk disabled",
                region.base
            );
            return;
        }
    }
    WALK_TRUSTED.store(true, core::sync::atomic::Ordering::Release);
    log::info!("heap walk self-test OK");
}

pub fn heap_integrity_ok(at: &str) -> bool {
    if crate::runtime::HAS_PSRAM
        || !WALK_TRUSTED.load(core::sync::atomic::Ordering::Acquire)
    {
        return true;
    }
    for region in C3_HEAPS {
        if !walk_heap(*region, true, at) {
            log::error!("heap integrity FAILED at {at}: skipping heap/task walks");
            return false;
        }
    }
    true
}

/// TEMPORARY DIAGNOSTIC (PR #15 C3 triage): full task census (name, stack
/// base, high-water mark, state) at each telemetry point. The corrupt header
/// reads 0xa5a5a5a5 -- the FreeRTOS stack-fill pattern -- so a task created
/// in the fault window with a stack overlapping live heap is the prime
/// suspect; the census proves or kills that theory in one run (a new task
/// appears, with a stack covering the slot, or the count/names never move).
/// Revert before merge.
pub fn log_task_census(at: &str) {
    // SAFETY: standard FreeRTOS state query; the array is sized from the
    // live task count plus margin, output truncated to what fits.
    unsafe {
        let n = sys::uxTaskGetNumberOfTasks() as usize;
        let cap = n.saturating_add(4).min(48);
        let mut states: Vec<sys::TaskStatus_t> = Vec::with_capacity(cap);
        let total =
            sys::uxTaskGetSystemState(states.as_mut_ptr(), cap as u32, core::ptr::null_mut());
        states.set_len((total as usize).min(cap));
        log::info!("tasks at {at}: count={total}");
        for st in &states {
            let name = core::ffi::CStr::from_ptr(st.pcTaskName.as_ptr()).to_string_lossy();
            log::info!(
                "task at {at}: {name} base={:#x} hwm={} state={}",
                st.pxStackBase as usize,
                st.usStackHighWaterMark,
                st.eCurrentState as u32,
            );
        }
    }
}

pub fn log_memory_profile(at: &str) {
    struct Opt(Option<u32>);
    impl std::fmt::Display for Opt {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                Some(v) => write!(f, "{v}"),
                None => write!(f, "-"),
            }
        }
    }
    // A corrupted heap can fault inside the walks below (Load access fault in
    // tlsf_walk_pool once, in prvSearchForNameWithinSingleList twice, always
    // just after the first ServerAck). Bail out instead: surviving past the
    // telemetry shows whether the C3 can connect with the corrupted words
    // unread, which bounds the blast radius for the writer hunt.
    if !heap_integrity_ok(at) {
        return;
    }
    log_task_census(at);
    let main_stack = stack_free_min(c"wa-main");
    let blocking_stack = stack_free_min(c"wa-blocking");
    let transport_stack = stack_free_min(c"ws-transport");
    let nvs_stack = stack_free_min(c"wa-nvs");
    // A corrupted heap can fault inside the TLSF walk below, before the normal
    // memory line is emitted. Preserve stack evidence first in emulator runs.
    #[cfg(feature = "qemu")]
    log::info!(
        "stack before heap at {at}: wa-main={} wa-blocking={} ws-transport={} wa-nvs={}",
        Opt(main_stack),
        Opt(blocking_stack),
        Opt(transport_stack),
        Opt(nvs_stack),
    );
    let cap = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    let free = unsafe { sys::heap_caps_get_free_size(cap) };
    let largest = unsafe { sys::heap_caps_get_largest_free_block(cap) };
    log::info!(
        "memory at {at}: internal heap {free} free, largest block {largest}; \
         stack never-used wa-main={} wa-blocking={} ws-transport={} wa-nvs={}",
        Opt(main_stack),
        Opt(blocking_stack),
        Opt(transport_stack),
        Opt(nvs_stack),
    );
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
    let stack_wa_blocking = stack_free_min(c"wa-blocking");
    let stack_wa_nvs = stack_free_min(c"wa-nvs");
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
        "stack_wa_blocking_min": stack_wa_blocking,
        "stack_wa_nvs_min": stack_wa_nvs,
        "stack_ws_transport_min": stack_ws_transport,
        "rssi_dbm": rssi,
        "last_panic": last_panic(),
        "coredump": coredump_json(),
    })
    .to_string()
}
