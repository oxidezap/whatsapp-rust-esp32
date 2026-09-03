# ESP32-C3: running this firmware without PSRAM

The ESP32-S3 and ESP32-C5 give this firmware 8 MB of external RAM, and it uses
it freely: the whole Rust heap, a 256 KB executor stack, every worker stack. The
ESP32-C3 has none. Its ~400 KB of on-chip SRAM is the entire memory system, and
ESP-IDF hands about **314 KB** of that to the heap:

```text
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
| `wa-main` executor stack | 256 KB | 32 KB | The send path (reaction + quoted reply + edit) has the deepest frames in the firmware. Measured peak 21,608 B. |
| `wa-blocking` worker stack | 32 KB | 10 KB | Prekey batches: CPU-bound, not deep. Measured peak 3,504 B. |
| `ws-transport` stack | 16 KB | 10 KB | mbedTLS records and `tungstenite` framing; neither recurses. Measured peak 5,632 B. |
| `wa-nvs` worker stack | 32 KB | 6 KB | Internal DRAM on every board (see below). Measured peak 2,596 B. |
| tungstenite read/write buffers | 128 KB | 4 KB each | The chunk reads are issued in, not a message cap. |
| tungstenite frame / message cap | 16 MiB / 64 MiB | 48 KB / 64 KB | Above the largest legitimate message (28,205 B), far below what the heap serves. See below. |
| admin HTTP sessions | 16 | 4 | One browser tab; the QEMU suite drives it with one `curl` at a time. |

Every ESP32-C3 figure in that table is a measured peak with headroom, not an
estimate. How they were measured, and what they were before, is in **Sizing the
stacks from the measurement** below.

Plus, from `sdkconfig.defaults.esp32c3`: the ESP-IDF main task stack drops from
32 KB to 20 KB (it is held for the life of the firmware and is only large for the
one-time NVS replay), the default pthread stack from 32 KB to 8 KB, the Wi-Fi
buffer counts from 10/32/32 to 4/8/8, and mbedTLS gets `DYNAMIC_BUFFER` plus
asymmetric record buffers (16 KB in, 4 KB out) since it can no longer allocate
from external memory.

`wa-nvs` is the one stack that is internal DRAM on *every* board -- writing flash
disables the cache, so a stack in PSRAM would fault mid-write -- which makes it
the one worth measuring rather than guessing at. Its jobs are shallow and
bounded: put or delete one record, or erase a namespace, and none of them puts a
record on the stack (`read_blob` and `encode_record` both build `Vec`s). The boot
replay, the only thing that walks the whole store, runs on the ESP-IDF main task
before this worker exists. Measured peak is **2,596 bytes**, and it barely moves
with the stack it is given: 2,416 with a 32 KB stack, 2,564 with a 12 KB one
after a `DELETE /sessions` (the deepest job, `erase_namespace`) and the reboot
that follows, 2,596 with 6 KB. Shrinking the stack by 26 KB moved the peak by
180 bytes, which is the point: this worker's depth is bounded by what its jobs
do, not by what it is given. 6 KB keeps well over twice the observed peak and
hands the rest of that internal DRAM back to the heap; the PSRAM boards keep the
32 KB they were tested on.

## The two failures worth writing down

Both were found by booting the thing, not by reading it, and neither would have
shown up on a board with PSRAM.

**1. 128 KB, twice, from a library default.** After a clean boot, a working
Ethernet link, a completed TLS handshake and a completed WebSocket upgrade, the
firmware died on:

```text
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

```text
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

```text
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

**Turning the dynamic allocator off does not fix it either**, and finding that
out is what produced the number this port had been missing. Static record
buffers are taken once inside the handshake, when the heap is least fragmented,
which sounds strictly better. It is not, because of what the heap actually looks
like by then:

```text
I (4415) whatsapp_esp32::transport: WS thread: internal heap 53332 free, largest block 31744
```

**53 KB**, not the 194 KB the boot-time `Free heap:` line reports -- that line is
printed before the bot is built. Holding ~21 KB of record buffers for the life of
the session out of 53 KB left the OpenCores MAC unable to allocate receive
buffers:

```text
opencores.emac: no mem for receive buffer          (×3.8 million)
task_wdt: Task watchdog got triggered ... CPU 0: emac_rx
```

A 206 MB serial log and a ten-minute job timeout, instead of the previous
abort at seven seconds. `CONFIG_MBEDTLS_DYNAMIC_BUFFER` is therefore back on: a
peak that fits is worth more here than a peak that is early.

## Sizing the stacks from the measurement

**The constraint is neither knob**, and instrumenting it settled the sizing
question this port had been answering by reasoning. `metrics::log_memory_profile`
prints free bytes, the largest free block and every worker stack's never-used
bytes on each WebSocket connect and each read error. One run:

```text
memory at websocket connect:     heap  75236 free, largest 49152; never-used wa-main=43932 wa-blocking=19584 ws-transport=8340 wa-nvs=9808
memory at websocket connect:     heap  49664 free, largest 26624; never-used wa-main=43932 wa-blocking=19584 ws-transport=8336 wa-nvs=9756
E Dynamic Impl: alloc(16749 bytes) failed
memory at websocket read error:  heap  13420 free, largest  7168; never-used wa-main=43932 wa-blocking=16976 ws-transport=6656 wa-nvs=9692
memory at websocket connect:     heap  41384 free, largest  8192
memory at websocket connect:     heap  27540 free, largest  8192
memory allocation of 4096 bytes failed
```

Two things fall out of it.

**The stacks were two to six times larger than anything they use.** Every one was
a `by_ram` constant picked by reasoning; these are the measured peaks:

| stack | was | measured peak | now |
| --- | --- | --- | --- |
| `wa-main` | 65,536 | **21,604** | 40,960 |
| `wa-blocking` | 20,480 | **3,504** | 12,288 |
| `ws-transport` | 12,288 | **5,632** | 10,240 |
| `wa-nvs` | 12,288 | **2,596** | 8,192 |

That returns **38 KB** to the heap while keeping at least 85% headroom on the
tightest of them, and it is the lever the two TLS attempts were substitutes for.
`/metrics` had been reporting the same high-water marks all along, but the
end-to-end run never reads it, so the one build where the sizing bites is the
build where nobody was looking.

**The heap also decays across reconnections** -- 75 KB, 49 KB, 41 KB, 27 KB --
and the largest free block sticks at 8,192 for the last two, which is
fragmentation rather than a shortage of total bytes.

## What the resized stacks bought, and the cap that undid it

With the four stacks sized from their measured peaks, the ESP32-C3 **completes
pairing**: QR, the `515` restart, re-authentication at gen=2 and again at gen=4.
No allocation failure appears anywhere in the log. The heap at connect is a
different machine from the one that kept aborting:

```text
1st connect: heap 126,512 free, largest block 114,688
2nd connect: heap 113,504 free, largest block  73,728
```

against 75,236 / 49,152 before the resize, and 53,332 / 31,744 before that.

The run still failed, and on a cap this port set itself:

```text
WS read error: Space limit exceeded: Message too long: 28205 > 8192
```

**28,205 bytes is the `<iq xmlns="abt"><props/>` response** -- the AB-props
table. `fetch_props` requests it unconditionally during every login's background
initialisation, and its delta form is only valid once a full response has
succeeded, so there is no way to not receive it. It is a legitimate message, and
at the moment it arrives the heap has 73,728 bytes contiguous: it fits with room
to spare. Only the 8 KB cap rejected it, and the reconnect loop that followed is
what ground the largest free block down to 8,704 -- a worse failure than the one
the cap was meant to prevent.

So the caps are now set from that measurement rather than from caution: 48 KB
per frame and 64 KB per message, above the largest legitimate message with room
for `BytesMut`'s doubling, and far below what the heap can serve. Still ~1000x
tighter than tungstenite's defaults, which is what makes a hostile peer a clean
protocol error instead of an abort.

## Naming the allocation instead of guessing at it

With the stacks resized the mbedTLS failure was gone, and the abort moved to
`memory allocation of 32300 bytes failed` -- Rust's allocator, not mbedTLS. The
per-frame heap note makes the run-up readable:

```text
<-- WS recv  40 bytes  heap=71208/57344
--> WS send  57 bytes  heap=53468/40960
--> WS send 292 bytes  heap=50344/34816
<-- WS recv  34 bytes  heap=50180/34816
memory allocation of 32300 bytes failed
```

50 KB free, 34.8 KB contiguous, and a single request for 32.3 KB. The stack dump
names it, resolved against the CI firmware with `addr2line`:

```text
0x4202811e  <bytes::bytes_mut::BytesMut>::reserve_inner
0x42161fa6  Esp32TransportFactory::create_transport::{closure#0}   # the ws-transport thread
0x42075b28  std::alloc::rust_oom::{closure#0}
```

It is `tungstenite`'s read buffer doubling: `BytesMut::reserve_inner` grows by
`max(len + additional, cap * 2)`, and **16,150 x 2 = 32,300** exactly. Because it
reallocates and copies, it needs the whole 32 KB *contiguous* -- which the heap
had a moment earlier and had lost by the time it asked. `read_buffer_size` does
not bound this: it is the chunk reads are issued in, not the buffer a whole
message accumulates into, and a full 16 KB TLS record's worth of WebSocket
payload is what pushes it over.

So the remaining work is contiguity, not total bytes, and the second round of
stack cuts is sized for it. The peaks measured twice, on the resized build:

| stack | after round 1 | measured peak | now |
| --- | --- | --- | --- |
| `wa-main` | 40,960 | 21,608 | 32,768 |
| `wa-blocking` | 12,288 | 896 | 10,240 |
| `ws-transport` | 10,240 | 3,956 | 10,240 (left alone) |
| `wa-nvs` | 8,192 | 2,532 | 6,144 |

`ws-transport` keeps its size deliberately: it is the thread that ran out of
*heap*, and shrinking its stack would confuse the next measurement for nothing.
The admin server's session table also drops from 16 to 4 without PSRAM -- the
dashboard is one browser tab and the suite drives it with one `curl` at a time.

The general lesson is the one this port keeps repeating: the numbers that matter
here are the *peak contiguous* ones, not the totals. 194 KB free with a 28 KB
largest block is a different machine from 194 KB free with a 100 KB one.

## The board

`partitions.csv` is unchanged, so a C3 board needs **at least 8 MB of flash**
(4.11 MB app, 1 MB `wa_store`) and the table as written assumes 16 MB. The common
4 MB devkits (ESP32-C3-DevKitM-1, DevKitC-02) cannot hold this image at all.
