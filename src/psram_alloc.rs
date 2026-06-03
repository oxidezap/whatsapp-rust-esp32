//! Global allocator routing the entire Rust heap to PSRAM.
//!
//! Internal DRAM (~300 KB) is the scarce resource and is the only place FreeRTOS
//! objects (mutexes/semaphores, created lazily by `std::sync` primitives) and DMA
//! buffers can live. The default malloc heuristic places small allocations in
//! internal DRAM; over a session the Rust heap's many small live objects accumulate
//! there and drain it (measured: ~205 KB), so a later `xSemaphoreCreateMutex` fails
//! with EAGAIN and std panics. Sending every Rust allocation to the 8 MB PSRAM pool
//! removes that pressure entirely. Instructions/rodata already execute from PSRAM
//! here (SPIRAM_FETCH_INSTRUCTIONS/RODATA), so a hot SPI path is already accepted.

use core::alloc::{GlobalAlloc, Layout};

use esp_idf_svc::sys;

struct PsramAllocator;

unsafe impl GlobalAlloc for PsramAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // aligned_alloc honors any power-of-two alignment Layout yields (plain
        // heap_caps_malloc only guarantees word alignment, too weak for align>4 types).
        // Clamp up to 4: over-aligning a 1/2-byte request is harmless and stays on the
        // well-trodden alignment path. heap_caps_free handles any alignment on dealloc.
        let align = layout.align().max(4);
        let caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
        let p = unsafe { sys::heap_caps_aligned_alloc(align, layout.size(), caps) } as *mut u8;
        if !p.is_null() {
            return p;
        }
        // PSRAM exhausted, or called before PSRAM is mapped very early in boot: fall
        // back to the default heap so an allocation never spuriously fails.
        unsafe {
            sys::heap_caps_aligned_alloc(align, layout.size(), sys::MALLOC_CAP_DEFAULT) as *mut u8
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // heap_caps_free frees both regular and aligned allocations (the dedicated
        // heap_caps_aligned_free is deprecated in favor of it).
        unsafe { sys::heap_caps_free(ptr as *mut core::ffi::c_void) };
    }
}

#[global_allocator]
static ALLOCATOR: PsramAllocator = PsramAllocator;
