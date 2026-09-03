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

No logic in `src/` branches on chip identity -- chip names appear only in
comments explaining why a number is what it is. A board declares PSRAM or does
not (`scripts/boards.sh` layers `sdkconfig.psram` for the boards that have it),
and the firmware follows. That is why adding a fourth board is still a row in
`scripts/boards.sh` plus its `sdkconfig.defaults.<mcu>` overlay, even though the
C3 differs from the others in the deepest possible way.

## What `by_ram` actually decides

| Item | PSRAM | ESP32-C3 | Why |
| --- | --- | --- | --- |
| `wa-main` executor stack | 256 KB | 64 KB | The send path (reaction + quoted reply + edit) has the deepest frames in the firmware. |
| `wa-blocking` worker stack | 32 KB | 20 KB | Prekey batches: CPU-bound, not deep. |
| `ws-transport` stack | 16 KB | 12 KB | mbedTLS records and `tungstenite` framing; neither recurses. |
| `wa-nvs` worker stack | 32 KB | 12 KB | Internal DRAM on every board (see below), so the one stack worth measuring. Peak use 2,564 B. |
| tungstenite read buffer | 128 KB | 8 KB | See below. |
| tungstenite write buffer | 128 KB | 8 KB | See below. |

Plus, from `sdkconfig.defaults.esp32c3`: the ESP-IDF main task stack drops from
32 KB to 20 KB (it is held for the life of the firmware and is only large for the
one-time NVS replay), the default pthread stack from 32 KB to 8 KB, the Wi-Fi
buffer counts from 10/32/32 to 4/8/8, and mbedTLS gets asymmetric record
buffers (16 KB in, 4 KB out) -- but deliberately not `DYNAMIC_BUFFER`, for the
reasons below -- since it can no longer allocate
from external memory.

`wa-nvs` is the one stack that is internal DRAM on *every* board -- writing flash
disables the cache, so a stack in PSRAM would fault mid-write -- which makes it
the one worth measuring rather than guessing at. Its jobs are shallow and
bounded: put or delete one record, or erase a namespace, and none of them puts a
record on the stack (`read_blob` and `encode_record` both build `Vec`s). The boot
replay, the only thing that walks the whole store, runs on the ESP-IDF main task
before this worker exists. Measured peak is **2,564 bytes**: 2,416 with a 32 KB
stack, and 2,564 with a 12 KB one after a `DELETE /sessions` (which runs the
deepest job, `erase_namespace`) and the reboot that follows it. Shrinking the
stack by 20 KB moved the peak by 148 bytes, which is the point: this worker's
depth is bounded by what its jobs do, not by what it is given. 12 KB keeps nearly
four times the observed peak and hands that 20 KB of internal DRAM back to the
heap; the PSRAM boards keep the 32 KB they were tested on.

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
- The `Bot` is built, the transport connects over TLS and completes the
  WebSocket handshake.
- `DELETE /sessions` erases a namespace on the `wa-nvs` worker, the firmware
  reboots itself, and the store comes back `device=true`: what the first boot
  wrote survived in the emulated flash.

Heap, with the network up, the dashboard bound, the `Bot` built and the transport
connected:

| | Free | All-time low |
| --- | --- | --- |
| At `Free heap:` in the boot log | 173,880 (166,396 internal) | -- |
| Steady state | 64,680 | **57,016** |

Stack high-water marks from `GET /metrics` (bytes still free at the deepest point):

| Thread | Size | Free at peak | Used |
| --- | --- | --- | --- |
| `wa-main` | 64 KB | 45,004 | 20,532 |
| `wa-nvs` | 12 KB | 9,724 | 2,564 |
| `ws-transport` | 12 KB | 6,588 | 5,700 |
| `wa-blocking` | 20 KB | 19,568 | 912 (no prekey batch yet) |

Not verified, and it matters:

- **No real hardware.** Nobody has flashed this to a C3.
- **QEMU has no radio.** The emulated board uses Ethernet, so `bring_up_wifi` is
  never exercised there, and the ~40 KB the Wi-Fi driver holds is not in any of
  the heap figures above. That is the single biggest open question for a real
  board, and it is why the Wi-Fi buffer counts in the overlay are trimmed on
  reasoning rather than on measurement.
- **The full pairing flow** needs the mock server CI runs; the `qemu-e2e` job is
  what closes that gap -- and it is what found the one bug no local run could.
  See below.

## What the end-to-end run found that a local boot could not

The `qemu-e2e` runs on this chip got further than anything reachable locally and
then died -- twice, with the same allocation, and the first diagnosis of it was
wrong. Worth recording in full, because both the bug and the mistake are the
shape of the problem on a chip this size:

```
I (6384) whatsapp_rust::prekeys: Server missing prekeys (persisted flag), uploading.
E (6564) Dynamic Impl: alloc(16749 bytes) failed
E (6564) esp-tls-mbedtls: read error :-0x7F00
...
memory allocation of 4192 bytes failed
abort() was called at PC 0x420746ad on core 0
```

Pairing itself succeeded: QR flow, all the server-side validation, the `515`
reconnect, re-authentication. It fell over afterwards, on the prekey upload.

**The wrong diagnosis.** 16,749 reads like `MBEDTLS_SSL_IN_CONTENT_LEN` (16384)
plus record overhead, so the first fix lowered that value to 8192. The next run
asked for 16,749 again. Disassembling `esp_mbedtls_add_rx_buffer` in the failing
firmware settles it:

```
lw   a5,108(s0)          # ssl->in_len -- the record header just read off the wire
lbu  a4,0(a5)            # length, big-endian
lbu  a5,1(a5)
...
addi s1,a5,333           # size = record_length + 333
addi s5,s1,8             #      + 8
jal  mbedtls_calloc
```

The size is `record_length + 341`, taken from the header the **peer** sent:
16,749 is 16408 + 341, not 16384 + 365. No sdkconfig value bounds it, which is
why lowering the ceiling changed nothing.

**The actual fix** is to stop asking. `CONFIG_MBEDTLS_DYNAMIC_BUFFER` frees the
receive buffer after every record and allocates the next one on demand; that is
the feature ESP-IDF added for exactly this class of chip, and it is now off here.
Two reasons:

- *Heap.* An on-demand 16 KB contiguous request lands mid-session, against a heap
  that by then holds a live TLS session, a tearing-down one and the protocol on
  top of both. Static buffers are taken once inside the handshake, when the heap
  is at its least fragmented, and never asked for again. That costs ~21 KB held
  for the life of the session instead of ~4 KB between records; half of it is
  paid back by dropping the WebSocket read and write buffers from 8 KB to 4 KB.
- *Correctness.* This transport polls with a 100 ms read timeout, so
  `esp_tls_conn_read` returns `WANT_READ` in the middle of a record routinely, and
  rebuilding the receive buffer around a partially-read header is the fragile path
  in that implementation. A claimed record length of 16408 on a connection whose
  largest frame the server ever sent was 634 bytes is what a desynchronised header
  looks like.

The incoming ceiling therefore goes back to the full 16384: with static buffers a
larger record is a hard protocol failure, and the ceiling is the peer's to choose.
RFC 6066's max_fragment_length would cap it at 4096, but it needs the server to
honour the extension and neither peer here does.

Every WebSocket connect now logs free bytes *and* the largest free block, because
the second number is the one that decided this and only the first was being
printed.

The general lesson is the one this port keeps repeating: the numbers that matter
here are the *peak contiguous* ones, not the totals. 194 KB free with a 28 KB
largest block is a different machine from 194 KB free with a 100 KB one.

## The board

`partitions.csv` is unchanged, so a C3 board needs **at least 8 MB of flash**
(4.11 MB app, 1 MB `wa_store`) and the table as written assumes 16 MB. The common
4 MB devkits (ESP32-C3-DevKitM-1, DevKitC-02) cannot hold this image at all.
