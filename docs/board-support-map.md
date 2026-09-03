# Board support map

What the firmware needs from a chip, which Espressif parts meet it, which ones
could with work, and which emulator can stand in for each in CI.

The three boards in [the README's Hardware table](../README.md#hardware)
(ESP32-S3 N16R8, ESP32-C5 N16R8, ESP32-C3) are the ones actually built; the S3
and the C3 are also booted, in the `qemu-e2e` CI job. This document was written
as a survey to decide **which board to add next and on which emulator**, and §5
is the one row that has since been acted on -- the ESP32-C3 port. Everything else
is still survey. Rows are marked *measured* (a number from this repo),
*documented* (a vendor spec) or *estimated* (an inference from the two).

## 1. The requirement profile

Every number is from this tree, so it moves when the firmware moves.

| Requirement | Value | Where it comes from |
| --- | --- | --- |
| Mapped PSRAM | **≥ 2 MB, 4 MB comfortable** | *measured*: the dashboard sample in the README reports `psram_free: 3463948` out of the ~4 MB the S3 maps for data, i.e. ~0.5 MB live at idle, on top of the stacks below. |
| PSRAM presence | **not required**, but everything above assumes it | *measured*: with PSRAM the Rust heap goes to `MALLOC_CAP_SPIRAM` (`src/psram_alloc.rs`) and the executor takes a 256 KB stack; without it both come out of internal DRAM at the sizes in `runtime::by_ram`. The ESP32-C3 does this and runs, at ~314 KB of total heap (§5). |
| Flash | **≥ 8 MB** (16 MB assumed by `partitions.csv`) | *measured*: 4.5 MB app image (`App/part. size 4,503,680`), a 0x4C0000 factory partition and a 1 MB `wa_store` NVS partition. 4 MB parts are out without a rewrite. |
| Internal DRAM | **~300 KB, of which ≥ 64 KB in stacks** | *measured*: `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768` plus the 32 KB **internal** `wa-nvs` stack (`src/storage.rs`; it must be internal because writing flash disables the cache). `internal_min_free` runs at a few KB on the S3. |
| PSRAM-resident stacks | **~342 KB** | *measured*: `wa-main` 256 KB + `wa-blocking` 32 KB + `ws-transport` 16 KB + `httpd` 6 KB (`src/main.rs`, `src/runtime.rs`, `src/transport.rs`, `src/admin.rs`), all with `MallocCap::Spiram`. Needs `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM`. |
| Radio | **Wi-Fi station**, or Ethernet | *measured*: `bring_up_wifi` vs. the `qemu` feature's `bring_up_ethernet` (`src/main.rs`). Bluetooth is never used. |
| Toolchain | a **Rust `*-espidf` std target** | *measured*: `.cargo/config.toml`, `build-std`, `ldproxy`. |
| ESP-IDF | **v5.5.5** today | *measured*: `.cargo/config.toml` and `scripts/build.sh`. |
| Cores | **1 is enough** | *measured*: only `Core::Core0` is ever named; the C5 is already single-core. |

Two consequences worth stating plainly, because they decide most of the table:

- **A 4 MB-flash part cannot hold this app**, and this is the hard one. The image
  is 3.9--4.5 MB depending on the chip, before the 1 MB store.
- **No PSRAM is survivable, but it is a different budget.** The rows above are
  the PSRAM profile; a chip without it trades the 8 MB external heap for whatever
  internal SRAM is left after Wi-Fi, lwIP and mbedTLS, which on the C3 is about
  314 KB. That is enough (§5), with no headroom to spare. When reading the table
  below, "no PSRAM" moves a chip from comfortable to tight, not from possible to
  impossible -- what still rules a chip out is flash size and the radio.

## 2. The whole family, scored

PSRAM support is taken from whether ESP-IDF publishes an *External RAM* guide for
the target (`api-guides/external-ram.html`); QEMU support from whether it
publishes a *QEMU Emulator* guide (`api-guides/tools/qemu.html`). Both checked
against ESP-IDF `latest`.

| Chip | Core / clock | Internal SRAM | PSRAM | Radio | Rust std target | Espressif QEMU | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **ESP32-S3** | Xtensa LX7 ×2, 240 MHz | 512 KB | quad + **octal**, 8 MB | Wi-Fi 4 + BLE | `xtensa-esp32s3-espidf` | **yes** | **shipping** — the reference board |
| **ESP32-C5** | RISC-V, 240 MHz | 384 KB | quad | Wi-Fi 6 dual-band + BLE | `riscv32imac-esp-espidf` | no | **shipping** — built in CI, never emulated |
| **ESP32** (classic) | Xtensa LX6 ×2, 240 MHz | 520 KB | quad, **4 MB mapped** | Wi-Fi 4 + BT/BLE | `xtensa-esp32-espidf` | **yes** (2 M/4 M PSRAM) | **best next target** |
| **ESP32-S2** | Xtensa LX7 ×1, 240 MHz | 320 KB | quad, up to 10.5 MB of address space | Wi-Fi 4, no BT | `xtensa-esp32s2-espidf` | no | plausible, tight on internal DRAM |
| **ESP32-C61** | RISC-V ×1, 160 MHz | 320 KB | quad, 2/8 MB | Wi-Fi 6 + BLE | `riscv32imac-esp-espidf` (as the C5) | no | plausible — the cheap C5 |
| **ESP32-P4** | RISC-V ×2, 360–400 MHz | 768 KB | up to **32 MB** | **none** (Ethernet MAC; Wi-Fi via ESP-Hosted) | `riscv32imafc-esp-espidf` | no | special case, see §4 |
| **ESP32-C6** | RISC-V ×1, 160 MHz | 512 KB | **none** | Wi-Fi 6 + BLE + 802.15.4 | `riscv32imac-esp-espidf` | no | plausible — **more** SRAM than the C3, but no QEMU machine |
| **ESP32-C3** | RISC-V ×1, 160 MHz | 400 KB | **none** | Wi-Fi 4 + BLE | `riscv32imc-esp-espidf` | **yes** | **shipping** — see §5 and [docs/esp32c3.md](esp32c3.md) |
| ESP32-C2 | RISC-V ×1, 120 MHz | 272 KB | **none** | Wi-Fi 4 + BLE | `riscv32imc-esp-espidf` | no | below the floor: 272 KB against the C3's 314 KB of heap |
| ESP32-H2 | RISC-V ×1, 96 MHz | 320 KB | **none** | BLE + 802.15.4, **no Wi-Fi** | `riscv32imac-esp-espidf` | no | blocked twice over |

Espressif's own answer on the C-series is that the C6 and its siblings "lack the
internal hardware to incorporate PSRAM into their memory map"; the C61 is the
variant that adds it back to compensate for a smaller SRAM.

Before the ESP32-C3 port that fact sorted this whole table, because the firmware
could not run without PSRAM. It no longer does. What sorts the table now is
**flash first** (nothing under 8 MB can hold the image), then **the radio**, and
only then internal SRAM -- where the C3's ~314 KB of heap is the working
reference for how little is enough. The rows above are updated accordingly: the
C6, with 512 KB of SRAM, has more room than the chip that is already shipping,
and is held back only by having no QEMU machine to prove it on.

## 3. Candidates, in the order worth attempting

### Tier 1 — ESP32 classic (ESP32-WROVER-E N16R8 or equivalent)

The only *new* chip that is both PSRAM-capable **and** emulated by Espressif's
QEMU, which is exactly the axis this exercise is about.

What is already free: same Xtensa `esp` toolchain, same espup install, same
ESP-IDF. The port is a row in `scripts/boards.sh` (triple, whether it has PSRAM,
its QEMU machine) plus a `sdkconfig.defaults.esp32` overlay -- `scripts/build.sh`
and `scripts/qemu.sh` take every board from that table and need no per-board arm.
CI is the third piece: a board only gets built or booted there if it has a row in
the `build` matrix, and an emulated one a row in `qemu-e2e` too.

What has to be checked, in order:

1. **4 MB of mapped PSRAM, not 8.** The ESP32 MMU maps at most 4 MB of PSRAM into
   the data address space at a time. Reaching past that needs `himem`, an
   ESP32-only bank-switching API: it reserves a window of 32 KB banks inside those
   4 MB and swaps which physical bank is visible there
   (`esp_himem_alloc` / `esp_himem_map` / `esp_himem_unmap`,
   `CONFIG_SPIRAM_BANKSWITCH_ENABLE`). It is not an option here, for three
   independent reasons: himem memory is not part of `heap_caps_malloc`, so
   `src/psram_alloc.rs` cannot hand out pointers from it and the 256 KB executor
   stack certainly cannot live in a bank that may be unmapped under it; enabling
   it *reduces* the malloc-able PSRAM, since the reserved window comes out of the
   same 4 MB; and QEMU does not emulate the PSRAM MMU anyway. So the budget on
   this chip is a flat 4 MB, even on an 8 MB module. *Estimated*: the ~0.5 MB live
   plus ~342 KB of stacks fit, but the margin is the whole question — instrument
   `psram_free` at the peak of first pairing, not at idle.
2. **No `SPIRAM_FETCH_INSTRUCTIONS` / `SPIRAM_RODATA`.** Those are S2/S3 knobs;
   the classic runs code from the flash cache only. This removes the S3's
   flash-vs-octal-PSRAM bus contention, so it may well behave *better*.
3. **The PSRAM cache workaround on old silicon.** ECO-level revisions before v3
   need the ESP-IDF PSRAM workaround, which costs internal DRAM. Pin a
   revision-3+ module and say so in the Hardware table.
4. **Internal DRAM.** 520 KB nominal, but the classic's DRAM is fragmented by the
   ROM and by the same Wi-Fi/lwIP/mbedTLS pressure the S3 already fights
   (`internal_min_free` in the low single-digit KB). This is the most likely
   failure, and the one QEMU will actually reproduce.
5. **Flash.** Needs a 16 MB module (or a re-cut `partitions.csv` for 8 MB).

QEMU-wise the classic is a near-clone of the existing job: `-M esp32`,
`open_eth` (so the existing `qemu` feature works unchanged), AES/SHA/RSA
emulated. Only `-m 2M`/`-m 4M` are accepted for PSRAM, and the PSRAM MMU is not
emulated — irrelevant here, since `himem` is not used.

### Tier 2 — ESP32-C61

Reads as a cheaper C5: same RISC-V ISA and therefore the same
`riscv32imac-esp-espidf` target and the same `sdkconfig` shape the C5 file already
has, with quad PSRAM up to 8 MB and Wi-Fi 6. 160 MHz instead of 240 MHz and
320 KB instead of 384 KB of SRAM: *estimated* slower pairing (key generation is
already the slowest step) and less internal-DRAM headroom, both of which the
existing 30 s watchdog and the PSRAM-first malloc routing were built to absorb.
Requires an 8 MB PSRAM / 16 MB flash module; the 2 MB PSRAM variants are the
marginal case, not the default.

No QEMU. Emulation has to come from Wokwi (§6).

### Tier 3 — ESP32-S2

Would be the interesting "how much weaker can it get while still being a real
port" data point: single core, no Bluetooth, 320 KB SRAM, Wi-Fi 4 only, and a
PSRAM address space that is generous on paper. Two concerns: 320 KB of internal
SRAM against a firmware whose internal low-water mark is already in the single
KBs on a 512 KB chip, and the fact that the common S2 modules ship **2 MB** of
PSRAM (ESP32-S2-WROVER), which is at the very bottom of the profile in §1.
No QEMU; Wokwi simulates the S2.

## 4. ESP32-P4 — the odd one out

It fails the radio requirement outright (no Wi-Fi, no BT) and passes everything
else by a wide margin: dual-core RISC-V at up to 400 MHz, 768 KB of internal
SRAM, up to 32 MB of PSRAM, and a 10/100 Ethernet MAC.

That MAC matters: the firmware **already has an Ethernet path**, `bring_up_ethernet`
behind the `qemu` feature. Splitting that feature into "how the network comes up"
(`wifi` / `ethernet`) and "we are inside an emulator" would make the P4 a real
board target rather than a hypothetical one — and would incidentally make the
QEMU flavor less of a special case. The Wi-Fi alternative is ESP-Hosted with a
companion C6, which is a much larger integration and not worth it here.

Worth doing only if a P4 board is actually on hand; as a CI target it is behind
Tier 1 and 2.

## 5. Deliberately weaker: the ESP32-C3, which now runs

This section used to be a cost estimate. It is now a report, because the port was
built and booted; the full account is [docs/esp32c3.md](esp32c3.md).

The C3 was the obvious target for the question the README opens with: 400 KB of
SRAM, no PSRAM, and — uniquely among the no-PSRAM parts — **first-class Espressif
QEMU support**, so it is both the smallest chip that could plausibly hold this
and the cheapest one to keep honest in CI.

What the estimate got right, and what it got wrong:

| Blocker | Estimated | What it actually took |
| --- | --- | --- |
| Global allocator | "delete the allocator" | One `#[cfg(esp_idf_spiram)]`. The library already only *defined* `PsramAllocator`; the firmware installs it. |
| `wa-main` stack | "~32–48 KB, means flattening the send path" | **64 KB, no restructuring.** The estimate was pessimistic. |
| Other stacks | "~100 KB total" | 20 KB blocking + 12 KB transport + 6 KB httpd + 12 KB `wa-nvs` (internal on every board; 32 KB where there is PSRAM). |
| Identity/session store | "read-through from NVS, no full cache" | **Not needed.** The RAM cache fits. |
| mbedTLS buffers | "4–8 KB content length, risky" | **The estimate was right.** 16 KB in was tried first and broke the end-to-end run: a 16,749-byte contiguous allocation this heap cannot serve. Now 8 KB in / 4 KB out, and "risky" is the honest label -- see [docs/esp32c3.md](esp32c3.md). |
| App image | "4.5 MB, fits fine" | 4.11 MB, 82.6% of the factory partition. |
| — | not foreseen | **`tungstenite`'s default 128 KB read *and* write buffers.** A single 128 KB allocation, invisible on 8 MB of PSRAM, is most of the C3's free heap; it killed the firmware after a *successful* TLS and WebSocket handshake. |

So the conclusion — "a no-PSRAM port is a different firmware, not a build
flavor" — was wrong, and instructively so. It is a build flavor. What made that
true was not cleverness in this port but the library split that landed first: the
allocator was already opt-in and every thread config already had a `_with`
variant, so there was nothing to restructure, only sizes to choose. The one real
bug was in a dependency's defaults, and no amount of reading would have found it.
Booting it did.

On the emulated C3 with the network up, the dashboard bound, the `Bot` built and
the transport connected, the all-time heap low-water mark is **57,016 bytes**.
That is the number to watch, and the honest caveat is that QEMU has no radio, so
it does not include what the Wi-Fi driver holds on a real board.

The C2 (272 KB, 120 MHz) and the H2 (no Wi-Fi at all) remain below the floor.

## 6. Emulators, and what each can actually cover

| Emulator | Chips | Network | PSRAM | Fit for this repo |
| --- | --- | --- | --- | --- |
| **Espressif QEMU** | ESP32, ESP32-C3, ESP32-S3 | OpenCores MAC (`open_eth`), user-mode slirp | S3: 2/4/8/16/32 MB, quad or octal. ESP32: **2 or 4 MB only**, MMU not emulated | what `scripts/qemu.sh` already drives; adding the classic is a second `-M` and a second `sdkconfig` |
| **Wokwi** | ESP32, S2, S3, C3, **C5 (alpha)**, C6, **C61**, H2, **P4 (beta)** | **simulated Wi-Fi** on every chip | 2/4/8 MB, quad or octal, per chip | the only way to emulate the C5 the repo already ships, and the only one that exercises `bring_up_wifi` instead of the Ethernet path. No Bluetooth (unused here). Needs a CI token. |
| **espressif/esp-emulator** | C3, C6, H2, P4 | Wi-Fi SoftAP (WPA2-PSK), OpenCores + Synopsys Ethernet, NAT/TAP | present, but "zero-initialized RAM rather than fully modeled" | interesting for a P4 lane; the loose PSRAM model makes it weak evidence for exactly the thing this firmware stresses |

### Suggested order of work

The ESP32-C3 lane is done (§5); what is left, in order:

1. **ESP32 classic on QEMU.** One `sdkconfig.defaults.esp32`, one `build.sh` arm,
   one `qemu-e2e` matrix axis over `-M esp32 -m 4M`. Reuses the whole existing
   pair → reboot → message script, and puts a genuinely different memory map and a
   second Xtensa core layout under the same test.
2. **A 2 MB-PSRAM S3 lane** (`-m 2M`, no new chip). Nearly free, and it is the
   experiment that establishes the PSRAM floor for every candidate above.
3. **ESP32-C5 on Wokwi.** Closes the standing gap that the C5 is built but never
   booted, and brings the Wi-Fi bring-up path under test for the first time. Track
   the alpha status: expect this to be the flaky lane.
4. **ESP32-C61**, once (3) works — same target triple, so mostly a matrix entry.
5. **ESP32-P4** only alongside an `ethernet`/`wifi` feature split, and only if the
   hardware is on hand.

Note that steps 1 and 3 answer different questions: QEMU checks the *memory map
and the crypto/storage stack*, Wokwi is the only lane that would check *Wi-Fi
association*. Neither replaces the board for RF behavior, timing against real
flash and PSRAM latency, or power — the caveat the README already makes.

## Sources

- ESP-IDF Programming Guide, per-target *Support for External RAM* and *QEMU Emulator* guides — https://docs.espressif.com/projects/esp-idf/en/latest/
- `espressif/esp-toolchain-docs`, QEMU notes for esp32, esp32c3, esp32s3 — https://github.com/espressif/esp-toolchain-docs/tree/main/qemu
- The rustc book, ESP-IDF platform support — https://doc.rust-lang.org/rustc/platform-support/esp-idf.html
- Espressif datasheets: ESP32-C5, ESP32-C61, ESP32-P4 — https://documentation.espressif.com/
- Wokwi ESP32 simulation docs — https://docs.wokwi.com/guides/esp32
- `espressif/esp-emulator` — https://github.com/espressif/esp-emulator
- ESP-IDF issue #13300, "ESP32-C6 Support for External RAM" — https://github.com/espressif/esp-idf/issues/13300
