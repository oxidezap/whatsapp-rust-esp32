# Board support map

What the firmware needs from a chip, which Espressif parts meet it, which ones
could with work, and which emulator can stand in for each in CI.

The two boards in [the README's Hardware table](../README.md#hardware) (ESP32-S3
N16R8, ESP32-C5 N16R8) are the ones actually built and, for the S3, booted in CI.
Everything below is a survey done to decide **which board to add next and on which
emulator**; nothing here has been built or booted yet. Rows are marked
*measured* (a number from this repo), *documented* (a vendor spec) or
*estimated* (an inference from the two).

## 1. The requirement profile

Every number is from this tree, so it moves when the firmware moves.

| Requirement | Value | Where it comes from |
| --- | --- | --- |
| Mapped PSRAM | **≥ 2 MB, 4 MB comfortable** | *measured*: the dashboard sample in the README reports `psram_free: 3463948` out of the ~4 MB the S3 maps for data, i.e. ~0.5 MB live at idle, on top of the stacks below. |
| PSRAM presence | **mandatory** | *measured*: `src/psram_alloc.rs` routes the whole Rust heap to `MALLOC_CAP_SPIRAM`, and `MAIN_TASK_STACK_SIZE` (`src/main.rs`) is a 256 KB stack no internal SRAM can provide. |
| Flash | **≥ 8 MB** (16 MB assumed by `partitions.csv`) | *measured*: 4.5 MB app image (`App/part. size 4,503,680`), a 0x4C0000 factory partition and a 1 MB `wa_store` NVS partition. 4 MB parts are out without a rewrite. |
| Internal DRAM | **~300 KB, of which ≥ 64 KB in stacks** | *measured*: `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768` plus the 32 KB **internal** `wa-nvs` stack (`src/storage.rs`; it must be internal because writing flash disables the cache). `internal_min_free` runs at a few KB on the S3. |
| PSRAM-resident stacks | **~342 KB** | *measured*: `wa-main` 256 KB + `wa-blocking` 32 KB + `ws-transport` 16 KB + `httpd` 6 KB (`src/main.rs`, `src/runtime.rs`, `src/transport.rs`, `src/admin.rs`), all with `MallocCap::Spiram`. Needs `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM`. |
| Radio | **Wi-Fi station**, or Ethernet | *measured*: `bring_up_wifi` vs. the `qemu` feature's `bring_up_ethernet` (`src/main.rs`). Bluetooth is never used. |
| Toolchain | a **Rust `*-espidf` std target** | *measured*: `.cargo/config.toml`, `build-std`, `ldproxy`. |
| ESP-IDF | **v5.5.5** today | *measured*: `.cargo/config.toml` and `scripts/build.sh`. |
| Cores | **1 is enough** | *measured*: only `Core::Core0` is ever named; the C5 is already single-core. |

Two consequences worth stating plainly, because they decide most of the table:

- **No PSRAM, no port.** Not a tuning problem — the allocator, the executor stack
  and the identity cache all assume an external heap.
- **A 4 MB-flash part cannot hold this app.** The image is 4.5 MB before the store.

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
| ESP32-C6 | RISC-V ×1, 160 MHz | 512 KB | **none** | Wi-Fi 6 + BLE + 802.15.4 | `riscv32imac-esp-espidf` | no | **blocked**: no PSRAM in the memory map |
| ESP32-C3 | RISC-V ×1, 160 MHz | 400 KB | **none** | Wi-Fi 4 + BLE | `riscv32imc-esp-espidf` | **yes** | **blocked**, but see §5 |
| ESP32-C2 | RISC-V ×1, 120 MHz | 272 KB | **none** | Wi-Fi 4 + BLE | `riscv32imc-esp-espidf` | no | blocked |
| ESP32-H2 | RISC-V ×1, 96 MHz | 320 KB | **none** | BLE + 802.15.4, **no Wi-Fi** | `riscv32imac-esp-espidf` | no | blocked twice over |

Espressif's own answer on the C-series is that the C6 and its siblings "lack the
internal hardware to incorporate PSRAM into their memory map"; the C61 is the
variant that adds it back to compensate for a smaller SRAM. That is the single
line that sorts this table.

## 3. Candidates, in the order worth attempting

### Tier 1 — ESP32 classic (ESP32-WROVER-E N16R8 or equivalent)

The only *new* chip that is both PSRAM-capable **and** emulated by Espressif's
QEMU, which is exactly the axis this exercise is about.

What is already free: same Xtensa `esp` toolchain, same espup install, same
ESP-IDF; the port is a `sdkconfig.defaults.esp32` file and a `--board esp32` arm
in `scripts/build.sh`, matching the "adding a board is adding that one file"
claim in the README.

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

## 5. Deliberately weaker: what would it take to reach a no-PSRAM chip

The user-facing question ("how far down the hardware ladder can this go?") points
at the C3: 400 KB of SRAM, no PSRAM, and — uniquely among the no-PSRAM parts —
**first-class Espressif QEMU support**. It is the cheapest possible emulated CI
target, and today it is unreachable. What stands in the way, quantified:

| Blocker | Today | Would have to become | Difficulty |
| --- | --- | --- | --- |
| Global allocator | whole Rust heap in PSRAM (`src/psram_alloc.rs`) | delete the allocator, live in ~300 KB of DRAM | mechanical, then everything else gets harder |
| `wa-main` stack | 256 KB, PSRAM | ~32–48 KB — means flattening the deep send path (reaction + quoted reply + edit) and boxing futures | **hard**, and the comment on `MAIN_TASK_STACK_SIZE` says the depth is real |
| Other stacks | 342 KB PSRAM + 64 KB internal | ~100 KB total | moderate |
| Identity/session store | replayed into a RAM cache at boot (`src/storage.rs`) | read-through from NVS, no full cache | moderate, and slower |
| mbedTLS buffers | `MBEDTLS_SSL_MAX_CONTENT_LEN=16384`, `MBEDTLS_EXTERNAL_MEM_ALLOC=y` | 4–8 KB content length, internal alloc | risky: WhatsApp frames may exceed it |
| App image | 4.5 MB | fits fine — C3 supports 16 MB flash | none |

*Estimated*: a no-PSRAM port is a **different firmware**, not a build flavor —
call it 300–400 KB of RAM budget against a stack that currently wants over 700 KB.
Flash is not the problem; RAM is. The honest intermediate step is not the C3 but
a **small-PSRAM** run: build for the S3 with `-m 2M` in QEMU, and separately with
`CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL` raised, and find where it actually breaks.
That experiment costs one CI matrix entry and tells you the real floor before
anyone buys a board.

The C2 (272 KB, 120 MHz) and the H2 (no Wi-Fi at all) are below the floor in every
version of this.

## 6. Emulators, and what each can actually cover

| Emulator | Chips | Network | PSRAM | Fit for this repo |
| --- | --- | --- | --- | --- |
| **Espressif QEMU** | ESP32, ESP32-C3, ESP32-S3 | OpenCores MAC (`open_eth`), user-mode slirp | S3: 2/4/8/16/32 MB, quad or octal. ESP32: **2 or 4 MB only**, MMU not emulated | what `scripts/qemu.sh` already drives; adding the classic is a second `-M` and a second `sdkconfig` |
| **Wokwi** | ESP32, S2, S3, C3, **C5 (alpha)**, C6, **C61**, H2, **P4 (beta)** | **simulated Wi-Fi** on every chip | 2/4/8 MB, quad or octal, per chip | the only way to emulate the C5 the repo already ships, and the only one that exercises `bring_up_wifi` instead of the Ethernet path. No Bluetooth (unused here). Needs a CI token. |
| **espressif/esp-emulator** | C3, C6, H2, P4 | Wi-Fi SoftAP (WPA2-PSK), OpenCores + Synopsys Ethernet, NAT/TAP | present, but "zero-initialized RAM rather than fully modeled" | interesting for a P4 lane; the loose PSRAM model makes it weak evidence for exactly the thing this firmware stresses |

### Suggested order of work

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
