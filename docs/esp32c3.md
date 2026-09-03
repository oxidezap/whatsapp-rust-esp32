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
| `wa-main` executor stack | 256 KB | 28 KB | The send path (reaction + quoted reply + edit) has the deepest frames in the firmware. Measured peak 21,608 B. |
| `wa-blocking` worker stack | 32 KB | 6 KB | Prekey batches: CPU-bound, not deep. Measured peak 3,404 B. |
| `ws-transport` stack | 16 KB | 8 KB | mbedTLS records and the crate's own WebSocket framing (`ws`); neither recurses. Measured peak 5,640 B. |
| `wa-nvs` worker stack | 32 KB | 4 KB | Internal DRAM on every board (see below). Measured peak 2,644 B. |
| WebSocket frame / message cap | 16 MiB / 64 MiB | 32 KB / 32 KB | Above the largest legitimate message (28,204 B). Refused from the frame header, before any allocation. See below. |
| admin HTTP sessions | 16 | 4 | One browser tab; the QEMU suite drives it with one `curl` at a time. |

The four **stack** figures are measured peaks with headroom, not estimates -- how
they were measured, and what they were before, is in **Sizing the stacks from the
measurement** below. The rest are configured limits *chosen from* measurements
rather than peaks themselves: the read buffer is sized so the largest observed
frame never forces a reallocation, the protocol caps sit above the largest
message the server actually sends, and the session count is what the dashboard
and the QEMU suite actually use.

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

## What the resized stacks bought, and the two buffer mistakes after it

With the four stacks sized from their measured peaks the ESP32-C3 **completes
pairing and uploads its prekeys**: QR, the `515` restart, re-authentication, and
`Successfully uploaded 50 pre-keys`. The heap at connect is a different machine
from the one that kept aborting:

```text
1st connect: heap 126,348 free, largest block 114,688
2nd connect: heap 113,512 free, largest block  73,728
```

against 75,236 / 49,152 before the resize and 53,332 / 31,744 before that.

Two buffer settings then had to be fixed, and both had been set by intuition
rather than by measurement.

**The caps were too tight.** Capping messages at 8 KB produced

```text
WS read error: Space limit exceeded: Message too long: 28205 > 8192
```

28,205 bytes is the `<iq xmlns="abt"><props/>` response -- the AB-props table,
which `fetch_props` requests unconditionally during every login's background
initialisation and whose delta form is only valid once a full one has succeeded.
It cannot be declined, it is legitimate, and it arrives when 73,728 bytes are
contiguous. The cap rejected it anyway, and the reconnect loop that followed
ground the largest free block to 8,704 -- worse than what the cap prevented.

**The read buffer, in both directions.** `read_buffer_size` reads like a
syscall-granularity knob. It is not: tungstenite's `FrameCodec` allocates
`in_buffer: BytesMut::with_capacity(read_buffer_size)` once and calls
`in_buffer.reserve(frame_len)` per frame header, and `BytesMut::reserve_inner`
reallocates to `max(len + additional, cap * 2)`, copying, so it needs the whole
new size contiguous.

At 4 KB the props frame asked for 4,096 + 28,205 = **32,301**, and the chip
aborted:

```text
0x4202815a  <bytes::bytes_mut::BytesMut>::reserve_inner
0x42161fa6  Esp32TransportFactory::create_transport::{closure#0}
0x42075b28  std::alloc::rust_oom::{closure#0}
```

Reading that as "the buffer is too small" and raising it to 40 KB made it
strictly worse. The buffer then held ~13 KB when the frame arrived, 13k + 28,205
just cleared 40,960, and the `cap * 2` term took over:

```text
memory allocation of 81920 bytes failed
```

81,920 is exactly 2 x 40,960. **The starting capacity is the floor of the next
request**, so raising it cannot remove the reallocation, only enlarge it. The
smallest possible request is what a small buffer gives, which is why the read
buffer is back to 4 KB. The remaining problem is not the size of the request but
what else holds the heap when it lands -- the prekey upload's device-state save
is in flight at that moment.

The same arithmetic sets the caps. A fragmented message accumulates in a `Vec`
that grows the same amortised way, so the worst-case contiguous request is about
**twice** the cap. At 32 KB per frame and per message that projects a ~64 KB
worst case, against the 110,592 bytes contiguous at the first connect, and still
clears the 28,205-byte AB-props response. It is arithmetic, not a tested
guarantee: the mock server sends that message as a single frame, so the
fragmented path has no regression coverage and the harness cannot inject one.

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

## What the measurement said once the log stopped hiding it

Every paragraph above this one was inference, and two of its conclusions were
wrong. The reason is worth stating plainly: on a crash the QEMU harness printed
`tail -n 80` of the serial log, and an ESP32 panic dumps registers plus hundreds
of lines of stack hex. The tail was *all* hexdump. The line that says why had
already scrolled past, so each round guessed a size, changed it, and read the
same silence back. `scripts/qemu.sh` now anchors on the crash signature and
prints the 60 lines *ending* at it, and the first run with that in place settled
the question:

```text
--> WS send  297 bytes                            heap=71984/59392
persistence_manager: Device state is dirty, saving to disk.
storage: device record: 2839 bytes (2828 payload) heap=20464/7168
<-- WS recv 28205 bytes                           heap=32560/13824
<-- WS recv    40 bytes                           heap=32560/13824
<-- WS recv   108 bytes                           heap=32560/13824
<-- WS recv   355 bytes                           heap=35704/13824
storage: device record: 2840 bytes (2829 payload) heap=35648/20480
memory allocation of 28775 bytes failed
```

Three things fall out of it.

**The device-state save is not the culprit.** The record is 2,839 bytes. The
theory that it held the contiguous block during the props read cannot survive a
2.8 KB measurement, and neither can the idea that fewer prekeys would shrink it:
`wacore::store::Device` holds identity and the key pairs, and the prekey pool
lives in the backend, not in the record. Cutting the count from 50 to 20 in fact
moved the crash *earlier*, which is evidence against the prekey burst being the
trigger at all.

**The 28,205-byte AB-props frame is received successfully.** It is not the
allocation that fails. `ws.read()` returns it, three small frames follow, and
only then does a **28,775**-byte request abort the chip -- on `wa-main`, while
that payload is being decrypted and parsed, with 35,648 bytes free but only
**20,480 contiguous**. So the peak is not one buffer but the payload plus a
second full-size copy of it, and the failure is contiguity again, one level up
from where the earlier rounds were looking.

**The heap collapses in a single 30 ms window.** Free goes 71,984 -> 20,464 and
the largest block 59,392 -> 7,168 between the 297-byte send and the next log
line. That window is upstream's background init: `node_io.rs` fires Props,
Blocklist, Privacy, Digest and Devices concurrently through `futures::join!`, so
the 28 KB props response lands while four other replies are in flight. There is
no knob on that from this crate.

Receiving a 28 KB WebSocket frame costs this firmware two blocks, not three: the
frame buffer, and a further ~28 KB to decode it. The split-off payload is *not*
a third copy -- `BytesMut::split_to` is refcounted, so it shares the frame
buffer's allocation, which is why that larger block stays alive for as long as
the message does.

That distinction matters, because the totals then say the C3 is not actually out
of memory. At the props read there are **73,004 bytes free**, and the two blocks
want 32,612 + 28,772 = **61,384**. It fails purely on *contiguity*: the frame
buffer is carved out of the 59,392-byte largest block, and what is left of it --
20,480 -- is about 8 KB short of the decode. The gap is single-digit kilobytes,
not the tens of kilobytes an earlier reading of this suggested.

## Sizing the stacks from the high-water marks, second pass

Which makes the per-task profile the fix rather than just the diagnosis. Logged
at the events that bracket the window, it reports how much of each stack has
*never been touched* since boot:

| stack | size | never used | peak used | now | margin |
| --- | --- | --- | --- | --- | --- |
| `wa-main` | 32,768 | 11,160 | 21,608 | 28,672 | 33% |
| `wa-blocking` | 10,240 | 6,836 | 3,404 | 6,144 | 80% |
| `ws-transport` | 10,240 | 4,600 | 5,640 | 8,192 | 45% |
| `wa-nvs` | 6,144 | 3,500 | 2,644 | 4,096 | 55% |

26,096 bytes across the four have never been written to. Reclaiming 12,288 of
them -- keeping at least a third clear above each measured peak -- is memory the
allocator never takes from the heap in the first place, so it widens the largest
free block rather than merely adding to the total. Against a shortfall of ~8 KB
in exactly that block, that is the lever the measurement points at.

If it turns out not to be enough, the next move is upstream rather than another
resize here: decoding the props response without materialising a second
full-size buffer would remove the 28,772-byte request altogether.

Where the 54 KB is *not*, at least, is settled. `Client::memory_report()` was
logged at those same events for one run and returned:

```text
--- Signal store caches ---
  signal_sessions:             0 entries          0 B
  signal_identities:           0 entries          0 B
  signal_sender_keys:          0 entries          0 B
  total estimated:        59 B
```

Fifty-nine bytes, with every queue and cache empty. Nothing upstream retains
that memory, so it belongs to the session's transient buffers -- mbedTLS and the
WebSocket -- not to a cache that could be bounded. The call was then removed:
it prints about forty lines per event, and three of those pushed the actual
run-up out of the sixty-line crash window, which is a bad trade once the answer
is known.

That same run also showed the other order this failure can take. Rather than the
decode, it was the **frame buffer** that failed, at 32,300, with the last frame
before it reporting `heap=62916/47104` -- a largest block comfortably above the
request. The missing piece is mbedTLS: with `CONFIG_MBEDTLS_DYNAMIC_BUFFER=y`,
`esp_mbedtls_add_rx_buffer` allocates the peer's record length plus 341 bytes for
the incoming record *before* tungstenite reserves. 47,104 less that ~16.7 KB
leaves about 30 KB, roughly 2 KB under the 32,300. So both shapes of the abort
are the same shortfall of a few kilobytes in one block, which is what makes the
stack reclaim worth measuring rather than dismissing.

## The reclaim worked, and it did not help

Those are usually assumed to be the same thing. Measured at the same point in
the run, before and after giving back 12,288 bytes of stack:

| | before | after |
| --- | --- | --- |
| at `--> WS send 292 bytes` | 73,004 / 59,392 | 84,688 / **69,632** |
| at the failing allocation | 35,836 / 20,480 | 48,028 / **20,480** |

The starting block gained the 10 KB it was supposed to, and total free at the
moment of failure gained 12 KB. The **largest block at that moment did not move
at all** -- 20,480 both times, against the same 28,772-byte request.

That answers "would a little more headroom close it", and the answer is no. The
block the decode needs is not bounded by how much memory is free; it is what
remains of the big region after the frame buffer (~32.6 KB) and the mbedTLS
record buffer (~16.7 KB) are carved out of it during the props read. Handing the
allocator more memory elsewhere does not change that remainder, so no further
trimming on this side will either. The stacks keep their reclaimed sizes because
the never-used figures justify them on their own, not because they fixed the C3.

Which leaves one specific piece of work: the 28,772-byte request is a second
full-size buffer materialised to decode a payload already held in memory.

## Two copies, and where each one came from

Following that request to its source named both halves of the problem, and
the port owned one of them after all.

The receive path was: a general-purpose WebSocket library grows its read buffer
to hold the frame (`reserve(28,204)` -> 32,612) and hands the payload out as a
refcounted view into that buffer, which it keeps for the life of the
connection. `whatsapp-rust`'s `FrameDecoder::feed(&data)` then copies the
payload into its own accumulation buffer (the 28,772), because a view it does
not own is all it was ever given -- `node_io.rs` even says so, in a comment
about dropping the view promptly. Two full-size copies, by construction, and
the second one is the abort.

So the port stopped using that library. `src/ws.rs` is the client half of RFC
6455 -- upgrade, binary frames, fragmentation, ping/pong, close, size caps
enforced from the header before anything is allocated -- and it does one thing
differently: each payload is allocated **once, at its declared size**, read
straight off the wire, and handed over as a `Bytes` with a single owner.
Nothing in the transport retains it. It is generic over `Read + Write` with the
random source injected, so its 13 tests run on a host with no ESP-IDF in sight,
and it removed `tungstenite`, `http`, `httparse` and `data-encoding` from the
tree while it was at it.

That alone turns a permanently-held 32,612 into a 28,204 that dies as soon as
the decoder has copied it. The decoder's copy is the other half, and it is
upstream: `FrameDecoder::feed_owned(Bytes)` adopts a uniquely-owned buffer via
`Bytes::try_into_mut` whenever a copy would have had to allocate, and decrypts
in place. With both halves the props frame costs one 28 KB buffer, not two.

## What the own client bought, and the wall behind it

The port-side half was measured on the first run that carried it. The S3 lane
passed end to end on the new client -- pairing, the 515 restart, persistence,
a message and its reaction -- so the framing is right. On the C3, at the same
point in the run:

| | tungstenite | own client |
| --- | --- | --- |
| at `<-- WS recv 28204 bytes` | 45,144 / 20,480 | **58,580 / 29,696** |
| the decoder's 28,772-byte copy | **aborted** | fits |
| the abort | `28772 bytes failed` | `64 bytes failed` |

The copy that had aborted four runs in a row now fits, and the firmware gets
past it for the first time. It then dies on a **64-byte** allocation with
52,924 free twenty milliseconds earlier: not fragmentation this time but
exhaustion, some 50 KB taken in one step by whatever consumes the props payload
next. That is `unpack_bytes`: the frame's format byte says the node is
zlib-compressed, and `decompress_zlib_pooled` inflates it.

What inflating costs, measured on a host with a counting allocator rather than
taken from a comment: `zlib_rs::Inflate::new(true, 15)` allocates **48,576
bytes** before it has produced a byte -- the 32 KB LZ77 window the compressor
used plus the state -- and the pool keeps that alive on the thread afterwards.
The output buffer is pre-sized to **twice the compressed length**, 56,408 here,
a guess tuned for multi-megabyte history-sync chunks. With the 28,204-byte
ciphertext still alive during inflation that is ~133 KB of demand against the
~93 KB this chip has free at that moment. No arrangement of the receive path
closes a gap of that shape; the decoder patch removes 28 KB of it and leaves
the wall where it is.

So the boundary is now exact. Everything up to the props response works on
the C3 without PSRAM, and the props response cannot be inflated in memory on
it. The two ways through are both upstream, and neither is a resize:

- **Stream it.** `wacore_binary::zlib_pool::InflateReader` already decompresses
  history sync incrementally, holding one record at a time; parsing the props
  node's children the same way would bound the peak at one property, not the
  whole response.
- **Make the fetch optional.** AB props are experiment flags; `fetch_props` is
  a best-effort background query whose failure is a warning. A client option to
  skip it on a target that cannot hold the answer is a configuration, not a
  workaround, and it is the smaller change by far.

## The board

`partitions.csv` is unchanged, so a C3 board needs **at least 8 MB of flash**
(4.11 MB app, 1 MB `wa_store`) and the table as written assumes 16 MB. The common
4 MB devkits (ESP32-C3-DevKitM-1, DevKitC-02) cannot hold this image at all.
