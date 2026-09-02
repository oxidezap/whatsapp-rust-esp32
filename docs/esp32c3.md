# ESP32-C3: running this firmware without PSRAM

The ESP32-S3 and ESP32-C5 give this firmware 8 MB of external RAM, and it uses
it freely: the whole Rust heap, a 256 KB executor stack, every worker stack. The
ESP32-C3 has none. Its ~400 KB of on-chip SRAM is the entire memory system, and
ESP-IDF hands about **314 KB** of that to the heap:

```
I (954) heap_init: At 3FC90080 len 0002FF80 (191 KiB): RAM
I (954) heap_init: At 3FCC0000 len 0001C710 (113 KiB): Retention RAM
I (954) heap_init: At 3FCDC710 len 00002950 (10 KiB): Retention RAM
```

This is what the port had to change, and what it did not.

## The one switch everything hangs off

`CONFIG_SPIRAM` in the sdkconfig. esp-idf-sys turns it into the `esp_idf_spiram`
cfg, and the firmware reads it in exactly three places:

| Where | With PSRAM | Without |
| --- | --- | --- |
| `src/main.rs`, `examples/minimal.rs` | install `PsramAllocator` | plain ESP-IDF allocator |
| `runtime::stack_caps` | `MallocCap::Spiram` | `MallocCap::Internal` |
| `runtime::by_ram(a, b)` | `a` | `b` |

There is no chip name anywhere in `src/`. A board declares PSRAM or does not
(`scripts/boards.sh` layers `sdkconfig.psram` for the boards that have it), and
the firmware follows. That is also why adding a fourth board stays a one-row
change even though the C3 differs from the others in the deepest possible way.

## What `by_ram` actually decides

| Item | PSRAM | ESP32-C3 | Why |
| --- | --- | --- | --- |
| `wa-main` executor stack | 256 KB | 64 KB | The send path (reaction + quoted reply + edit) has the deepest frames in the firmware. |
| `wa-blocking` worker stack | 32 KB | 20 KB | Prekey batches: CPU-bound, not deep. |
| `ws-transport` stack | 16 KB | 12 KB | mbedTLS records and `tungstenite` framing; neither recurses. |
| tungstenite read buffer | 128 KB | 8 KB | See below. |
| tungstenite write buffer | 128 KB | 8 KB | See below. |

Plus, from `sdkconfig.defaults.esp32c3`: the ESP-IDF main task stack drops from
32 KB to 20 KB (it is held for the life of the firmware and is only large for the
one-time NVS replay), the default pthread stack from 32 KB to 8 KB, the Wi-Fi
buffer counts from 10/32/32 to 4/8/8, and mbedTLS gets `DYNAMIC_BUFFER` plus
asymmetric record buffers (16 KB in, 4 KB out) since it can no longer allocate
from external memory.

The `wa-nvs` stack is unchanged at 32 KB of **internal** DRAM on every board: it
must be internal everywhere, because writing flash disables the cache and a stack
in PSRAM would fault mid-write.

## The two failures worth writing down

Both were found by booting the thing, not by reading it, and neither would have
shown up on a board with PSRAM.

**1. 128 KB, twice, from a library default.** After a clean boot, a working
Ethernet link, a completed TLS handshake and a completed WebSocket upgrade, the
firmware died on:

```
memory allocation of 131072 bytes failed
```

`tungstenite::protocol::WebSocketConfig` defaults to a 128 KB read buffer and a
128 KB write buffer. On an 8 MB external heap that is invisible. On the C3 it is
most of the free heap in a single request. Neither buffer bounds message size --
the read one is the chunk size reads are issued in, the write one is the point
past which tungstenite stops coalescing writes -- so 8 KB costs syscalls and
nothing else. The PSRAM boards keep the default, so the configuration they were
tested on is untouched.

**2. A console that went somewhere else.** The first C3 overlay copied the C5's
`CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y`. Under QEMU that produces a boot that
looks hung: the ROM prints (the ROM uses UART), then nothing at all from the
second-stage bootloader onwards. The C3 overlay stays on UART0, which is both
what the common devkits bring out through their USB-serial bridge and what
QEMU's `-serial` is. A board wired to the native USB endpoint wants that symbol
back, and then cannot be emulated.

A third, smaller one: `CONFIG_LWIP_MAX_SOCKETS=6` was too tight. ESP-IDF's httpd
reserves 3 sockets and refuses to start unless the table leaves room for its
`max_open_sockets` (4) on top, so the dashboard failed to bind and took startup
with it. Sockets are cheap; the window sizes are what cost DRAM. Back to 10.

## What is verified, and what is not

Verified by building and booting the `qemu` flavour on Espressif's QEMU
(`qemu-system-riscv32 -M esp32c3`):

- The firmware builds for `riscv32imc-esp-espidf` and links.
- The app image is **4,112,672 bytes**, 82.6% of the 0x4C0000 factory partition.
- It boots: custom partition table, 160 MHz, unicore, core dump armed.
- `NvsStore` opens the `wa_store` partition and replays it.
- DHCP over the emulated OpenCores MAC, SNTP, mDNS.
- The admin dashboard binds and registers every route.
- **Free heap after all of that: 173,880 bytes (166,396 internal).**
- The `Bot` is built, the transport connects over TLS and completes the
  WebSocket handshake.

Not verified, and it matters:

- **No real hardware.** Nobody has flashed this to a C3.
- **QEMU has no radio.** The emulated board uses Ethernet, so `bring_up_wifi` is
  never exercised there, and the ~40 KB the Wi-Fi driver holds is not in any of
  the heap figures above. That is the single biggest open question for a real
  board, and it is why the Wi-Fi buffer counts in the overlay are trimmed on
  reasoning rather than on measurement.
- **The full pairing flow** needs the mock server CI runs; the `qemu-e2e` job is
  what closes that gap.

## The board

`partitions.csv` is unchanged, so a C3 board needs **at least 8 MB of flash**
(4.11 MB app, 1 MB `wa_store`) and the table as written assumes 16 MB. The common
4 MB devkits (ESP32-C3-DevKitM-1, DevKitC-02) cannot hold this image at all.
