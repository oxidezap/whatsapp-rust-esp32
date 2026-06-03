//! Persistent crash diagnostics.
//!
//! A Rust panic's `file:line: message` string is copied into an RTC "no-init"
//! RAM region (`.rtc_noinit`, NOLOAD so it is not zeroed on a warm reset), so the
//! cause is readable from the dashboard after the chip reboots on panic. This
//! survives a reboot but NOT a power-off/brownout, which is the right scope,
//! since a panic is always a warm reboot. For power-off durability and hardware
//! exceptions (which carry no Rust string), the ESP-IDF core dump is the record.

use core::ptr::{addr_of, addr_of_mut};

use esp_idf_svc::sys;

/// The meaningful fields of an ESP-IDF core dump, read from flash at boot. Covers
/// both Rust panics and hardware exceptions (LoadProhibited, etc.).
pub struct CoredumpSummary {
    pub task: String,
    pub exc_pc: u32,
    pub exc_cause: u32,
    pub fault_addr: u32,
    pub backtrace: Vec<u32>,
    pub bt_corrupted: bool,
}

/// Read the core dump summary from the coredump flash partition (if a crash dump
/// is present), then erase it so it is reported only once and the next crash can
/// write cleanly. Returns `None` if there is no valid dump.
pub fn take_coredump_summary() -> Option<CoredumpSummary> {
    // SAFETY: read-only summary accessors over the coredump partition; called once
    // at boot. esp_core_dump_image_erase is the documented "consume" step.
    unsafe {
        if sys::esp_core_dump_image_check() != sys::ESP_OK {
            return None;
        }
        let mut s: sys::esp_core_dump_summary_t = core::mem::zeroed();
        let ok = sys::esp_core_dump_get_summary(&mut s) == sys::ESP_OK;
        let _ = sys::esp_core_dump_image_erase();
        if !ok {
            return None;
        }
        // exc_task is char[16] and may lack a NUL terminator.
        let raw: [u8; 16] = core::array::from_fn(|i| s.exc_task[i] as u8);
        let task = core::ffi::CStr::from_bytes_until_nul(&raw)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&raw).into_owned());
        let depth = (s.exc_bt_info.depth as usize).min(s.exc_bt_info.bt.len());
        Some(CoredumpSummary {
            task,
            exc_pc: s.exc_pc,
            exc_cause: s.ex_info.exc_cause,
            fault_addr: s.ex_info.exc_vaddr,
            backtrace: s.exc_bt_info.bt[..depth].to_vec(),
            bt_corrupted: s.exc_bt_info.corrupted,
        })
    }
}

const CRASH_MAGIC: u32 = 0xC0DE_DEAD;
const MSG_CAP: usize = 256;

#[repr(C)]
struct CrashLog {
    magic: u32,
    len: u16,
    _pad: u16,
    /// Free internal byte-addressable DRAM at the instant of panic, and the
    /// largest contiguous block. The crash is a FreeRTOS object (mutex/semaphore,
    /// which MUST live in internal DRAM) failing to allocate, so these two numbers
    /// (captured at the panic, not by periodic polling) are the real evidence.
    internal_free: u32,
    internal_largest: u32,
    msg: [u8; MSG_CAP],
}

#[link_section = ".rtc_noinit"]
#[used]
static mut CRASH_LOG: CrashLog = CrashLog {
    magic: 0,
    len: 0,
    _pad: 0,
    internal_free: 0,
    internal_largest: 0,
    msg: [0; MSG_CAP],
};

/// Alloc-light formatter into a fixed buffer (the panic hook must not depend on
/// the allocator being healthy).
struct FixedBuf {
    buf: [u8; MSG_CAP],
    len: usize,
}

impl core::fmt::Write for FixedBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        let n = b.len().min(MSG_CAP - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
        Ok(())
    }
}

/// Called from the panic hook: persist the formatted panic info to RTC RAM.
/// `info` is whatever the hook receives (`PanicHookInfo`), used via `Display`.
/// Field access goes through `addr_of_mut!` + raw `write`/`copy` to avoid taking
/// a reference into the `static mut` (rejected by the compiler).
pub fn record_panic(info: &impl core::fmt::Display) {
    use core::fmt::Write;
    // Read the internal-DRAM level FIRST, before this hook allocates anything, so
    // the numbers reflect the moment of failure. The failing allocation requested
    // INTERNAL|8BIT (the FreeRTOS object pool), so query that exact pool. These are
    // counter reads over a heap whose lock the failed alloc already released, so
    // they are safe to call here.
    let cap = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    let internal_free = unsafe { sys::heap_caps_get_free_size(cap) } as u32;
    let internal_largest = unsafe { sys::heap_caps_get_largest_free_block(cap) } as u32;
    let mut fb = FixedBuf {
        buf: [0; MSG_CAP],
        len: 0,
    };
    let _ = write!(fb, "{info}");
    // SAFETY: single-threaded panic context; publish msg/len then set the magic
    // LAST behind a fence so a half-written buffer is never seen as valid.
    unsafe {
        let msg_ptr = addr_of_mut!(CRASH_LOG.msg) as *mut u8;
        core::ptr::copy_nonoverlapping(fb.buf.as_ptr(), msg_ptr, fb.len);
        addr_of_mut!(CRASH_LOG.len).write(fb.len as u16);
        addr_of_mut!(CRASH_LOG.internal_free).write(internal_free);
        addr_of_mut!(CRASH_LOG.internal_largest).write(internal_largest);
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        addr_of_mut!(CRASH_LOG.magic).write(CRASH_MAGIC);
    }
}

/// Called once at boot: returns the previous run's panic message (if any) and
/// clears the magic so it is reported only once.
pub fn take_last_panic() -> Option<String> {
    // SAFETY: read-once at boot before any other task touches it.
    unsafe {
        if addr_of!(CRASH_LOG.magic).read() != CRASH_MAGIC {
            return None;
        }
        let len = (addr_of!(CRASH_LOG.len).read() as usize).min(MSG_CAP);
        let msg_ptr = addr_of!(CRASH_LOG.msg) as *const u8;
        let slice = core::slice::from_raw_parts(msg_ptr, len);
        let mut msg = String::from_utf8_lossy(slice).into_owned();
        let free = addr_of!(CRASH_LOG.internal_free).read();
        let largest = addr_of!(CRASH_LOG.internal_largest).read();
        let _ = core::fmt::Write::write_fmt(
            &mut msg,
            format_args!(" | internal-DRAM @panic: free={free} B, largest_block={largest} B"),
        );
        addr_of_mut!(CRASH_LOG.magic).write(0);
        Some(msg)
    }
}
