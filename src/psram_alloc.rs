//! Global allocator routing the entire Rust heap to PSRAM.
//!
//! Opt-in: the library only defines the type. A firmware installs it with
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: whatsapp_esp32::psram_alloc::PsramAllocator =
//!     whatsapp_esp32::psram_alloc::PsramAllocator;
//! ```
//!
//! which is what `src/main.rs` does, and which the memory figures in the README
//! assume. Without it the client still runs, but the internal-DRAM drain
//! described below returns.
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

/// See the module documentation.
pub struct PsramAllocator;

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

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // The default `realloc` is alloc + memcpy + free. TLSF can grow a block in
        // place (or absorb the free neighbour), and the receive path grows `Vec<u8>`
        // and `BytesMut` buffers many times per message, so let the heap try first.
        // Only for the word-aligned case: `heap_caps_realloc` promises nothing
        // beyond the heap's native alignment, so an over-aligned block keeps the
        // copying path, which re-runs `alloc` with the right alignment.
        if layout.align() > 4 {
            return unsafe { self.realloc_copy(ptr, layout, new_size) };
        }
        let caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
        let p = unsafe { sys::heap_caps_realloc(ptr as *mut core::ffi::c_void, new_size, caps) }
            as *mut u8;
        if !p.is_null() {
            return p;
        }
        // Same fallback as `alloc`: PSRAM is full, so accept the default heap. On
        // failure `heap_caps_realloc` leaves the original block untouched, which is
        // what the `GlobalAlloc` contract requires of a failed `realloc`.
        unsafe {
            sys::heap_caps_realloc(
                ptr as *mut core::ffi::c_void,
                new_size,
                sys::MALLOC_CAP_DEFAULT,
            ) as *mut u8
        }
    }
}

impl PsramAllocator {
    /// The library's default `realloc` body, kept for over-aligned blocks.
    unsafe fn realloc_copy(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract, and
        // `new_size` is non-zero and does not overflow `isize` when rounded up
        // to `layout.align()`, so this layout is valid.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let new_ptr = unsafe { GlobalAlloc::alloc(self, new_layout) };
        if !new_ptr.is_null() {
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
                GlobalAlloc::dealloc(self, ptr, layout);
            }
        }
        new_ptr
    }
}
