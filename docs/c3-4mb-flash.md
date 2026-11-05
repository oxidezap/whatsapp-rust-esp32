# Fitting the complete C3 firmware into 4 MiB flash

The qualified precomputed-curve production image is **3,792,880 bytes**, down
**395,488 bytes / 9.4%** from the 4,188,368-byte baseline. It fits the C3's
`0x3a0000` app slot with **8,208 bytes** left. The complete layout ends at
4 MiB and retains a **256 KiB WhatsApp store** and **64 KiB core dump**.
The two-device QEMU pairing, messaging and restored-session suite passed.
Physical C3 and production gateway qualification remain pending.

A companion [upstream draft](https://github.com/oxidezap/whatsapp-rust/pull/1861)
makes Dalek's basepoint table optional. The experimental compact-curve image
measures **3,753,776 bytes**, saving **434,592 bytes / 10.4%** overall and leaving
47,312 bytes of app margin. Two ordinary QEMU runs hit an interrupt watchdog
during initial pairing. All 252 Signal and 50 Noise tests pass in each backend configuration. Later
QEMU runs passed both the full two-device suite and a four-distinct-peer
workload, including the oldest session after reboot. The earlier watchdog
failures remain unexplained, so compact crypto is an explicit opt-in. The watchdog guards remain enabled.

## Physical budget

A 4 MiB device has 4,194,304 bytes. With the app starting at `0x10000`, a 64 KiB
core dump and the existing 1 MiB WhatsApp store, the application budget is
**3,014,656 bytes**, or `0x2e0000`. Getting the app below 4,000,000 bytes alone
does not make the board usable.

The existing table ends at `0x5e0000`, beyond a 4 MiB device, even if the app
itself were smaller. This branch adds a full-layout check to CI and QEMU image
generation, including partition alignment, overlap, physical flash bounds and
application fit. The C3 selects `partitions.esp32c3.csv`; S3 and C5 retain their
existing table.
The smaller store is an explicit capacity tradeoff, not a 1 MiB store compressed
into 256 KiB. There is no OTA slot.

## Measured production baseline

Base commit `1214e67`, protocol revision
`f7d9021505dcc64603ae38335a0dec5098848540`, ESP-IDF 5.5.5 and the pinned `esp`
1.98.0.0 toolchain. Built with:

```sh
scripts/build.sh --board esp32c3 --release
```

Default admin is enabled. This is a production build, without `mock-server`.
The environment used the official `mdns-v1.2.5` source tag because the component
registry download host was blocked. The manifest adaptation is local only.
Exact provenance, image/ELF hashes, archive accounting and largest symbols are
in [the measurement JSON](measurements/c3-flash-baseline.json).

`esp_idf_size` attributes **3,457,308 bytes**, about 82.5% of the app, to the
executable's LTO block. The large reductions must come from the Rust code.

| Named-symbol owner | Bytes |
| --- | ---: |
| `whatsapp_rust` | 831,360 |
| `waproto` | 480,488 |
| `core` | 471,832 |
| `wacore` | 233,520 |
| `whatsapp_esp32` | 153,340 |
| `sha2` | 64,736 |
| `curve25519_dalek` | 59,234 |

Owner attribution is approximate after LTO. It follows the first Rust path in
a symbol name and excludes anonymous literals. These are not independent
per-crate archive sizes. In particular, `core` includes generated protobuf
destructors, rather than only standard-library algorithms.

| Large symbol | Bytes |
| --- | ---: |
| `Message::merge_field` | 71,722 |
| SHA-512 compression | 49,724 |
| `Message::write_to` | 42,526 |
| `Message::compute_size` | 38,862 |
| `SyncActionValue::merge_field` | 37,956 |
| Dalek basepoint table | 30,720 |
| `Message::clone` | 22,214 |

Across named symbols mentioning `waproto`, destructors account for 73,240 bytes
and clones for 46,162 bytes. Two distinct `Message` destructor implementations
remain, at 6,412 and 6,218 bytes. These figures overlap the owner table.

## First compact result

| Measurement | App bytes | Change |
| --- | ---: | ---: |
| Production baseline | 4,188,368 | |
| Compact std + SHA-2 + RISC-V relaxation/save-restore | 3,819,600 | -368,768 / -8.8% |

The combined experiment keeps the complete schema, production verification,
admin, panic hook and core dump. These are combined measurements; individual
flag savings have not been isolated. See the [compact result and hashes](measurements/c3-flash-compact-rust.json).

Named SHA-2 symbols shrink from 64,736 to 3,644 bytes. `core` drops from 471,832
to 374,700, `waproto` from 480,488 to 426,726, and `whatsapp_rust` from 831,360
to 770,172. These attribution deltas describe linked symbols, not independent
feature ablations.

## Changes being measured

The selected compiler configuration passed the production size gate and the
full two-device QEMU workload with the precomputed curve backend:

- Build `std`, `core` and `alloc` with `optimize_for_size`. `opt-level = "z"`
  alone does not select their smaller formatting and sorting implementations.
- Use SHA-2's existing compact software backend on C3. The baseline's SHA-512
  compression routine alone is 49,724 bytes. This changes implementation, not
  hash formats. Host FIPS tests passed for SHA-256 and SHA-512, for `abc` and
  a streamed million-byte `a` input, forcing the compact software backend.
- Enable C3 linker relaxation and save/restore helpers. The baseline disassembly
  contains 113,071 `jalr` instructions. A minimal target object confirms that
  `+relax` emits `R_RISCV_RELAX`, absent from the default compilation. This makes
  long calls eligible for shortening; instruction counts are not a savings
  estimate. The final image must establish the actual delta.

The baseline already has machine outlining. Enabling another flag without a
measured reduction would not be an improvement. The target features are
nightly features and must stay tied to the pinned compiler and runtime tests.

No new savings are claimed for existing release log filtering, backtrace
removal, the common CA bundle, fat LTO, abort-on-panic, pinned upstream codecs
or generated pinned clones. The panic hook and flash core dump stay enabled.
The older [image-size report](app-image-size.md) describes a historical build.

## Selected configuration and rejected trials

The C/C++ helper build completed at **3,801,616 bytes**, another **17,984 bytes**
smaller. Its ELF/map match and archive accounting are recorded in
[the C/C++ measurement](measurements/c3-flash-compact-c.json).

An additional IR/machine outlining trial produced 3,795,472 bytes, only 6 KiB
smaller. Those extra compiler switches are not selected. Shared generics and
Cargo path trimming together bring the selected image to **3,792,880 bytes**,
another **8,736 bytes** smaller than the C/C++ helper build. The large generic
sharing savings reported for upstream host builds did not transfer to this C3
build. See the [validated ELF/map measurement](measurements/c3-flash-shared-trimmed.json).

The branch selects a 4 MiB C3 layout with a 256 KiB store and the existing
64 KiB core dump. The app slot is `0x3a0000`. Generated board and QEMU SDK configs
both select 4 MiB flash and a 44 KiB main stack. The latter addresses a real CI
failure: samples [2](https://github.com/oxidezap/whatsapp-rust-esp32/actions/runs/42115681394/job/127581804693)
and [3](https://github.com/oxidezap/whatsapp-rust-esp32/actions/runs/42115681394/job/127581804544)
completed pairing and the first message exchange but reported only 768 bytes
of main-stack headroom. The 2048-byte guard is unchanged.

Release builds also use Cargo path trimming. The C/C++ measurement still has
369 unique absolute Rust source paths occupying 35,516 bytes. Trimming removes
build-machine prefixes while retaining package-relative file/line locations;
the standalone host test verifies that those coordinates remain available.
The host location test passes; the measured image delta above includes trimming.

The C3 CI build now measures the production firmware without `mock-server`.
The separate QEMU build exercises the mock path. Both run the physical-flash
size gate against the C3-specific table.

## Runtime status and test setup

The first compact QEMU image booted, brought up Ethernet, loaded NVS and entered
the client run loop. Pairing then stopped at app-version discovery because the
emulator could not resolve `web.whatsapp.com` in this environment. The mock
builder now uses the pinned upstream `WA_WEB_VERSION` through the supported
`with_version` override. This is compiled only with `mock-server`; production
version discovery is unchanged.

The precomputed-backend suite used actual 4,194,304-byte flash images. It paired
two independent devices, exchanged messages, reactions, edits and quoted
replies, rebooted both with their stores intact, then repeated the exchange
without a new QR code. Stack minima were **4,864 bytes main**, **736 bytes
blocking**, at least **1,440 bytes NVS** and **2,576 bytes transport**. The guards
remain 2,048 bytes main and 512 bytes for the other threads. Final minimum
internal free heap was 35,892 bytes for A and 37,536 bytes for B; final largest
free blocks were 29,696 and 31,744 bytes. No panic or core dump was reported.

The final 256 KiB store images had 33 and 37 completely empty pages out of 64.
Raw NVS bitmaps contained 963 and 1,005 written entry slots, plus garbage and
unused slots. Entry slots include blob chunks and do not equal contacts or
messages. IDF's NVS parser found no corrupt/invalid entries. This proves the
bounded two-device workload only, not arbitrary group or contact capacity.

The compact backend also completed `scripts/qemu.sh capacity 4`. A paired main
device sent encrypted messages to four fresh peer identities, then both the main
device and the first peer rebooted. They restored their old identities without
a QR and exchanged a new message, reaction, edit and quoted reply using the
oldest Signal session. All stack/crash gates passed. Final main-device minimum
internal free heap was 52,908 bytes; consult the [raw metrics](measurements/c3-4mb-runtime.json)
for the exact per-phase values. This is a four-peer persistence check, not a
maximum-capacity or endurance claim.

The default features retain `precomputed-crypto`. The smaller candidate can be
built explicitly with:

```sh
scripts/build.sh --board esp32c3 --release --no-default-features --features admin
BOARD=esp32c3 scripts/qemu.sh build --no-default-features --features admin
BOARD=esp32c3 scripts/qemu.sh test
BOARD=esp32c3 scripts/qemu.sh capacity 4
```

CI builds both production profiles, boots the experimental compact backend
separately, and runs the four-peer workload on the default C3 profile. The
precomputed configuration at `0e26579` passed all board/QEMU builds and three
independent C3 runtime samples in [run 42126703762](https://github.com/oxidezap/whatsapp-rust-esp32/actions/runs/42126703762).
The dashboard job timed out during its cold browser/dependency setup; the
job budget is raised from 10 to 20 minutes. Dashboard JSON checks passed.

The profiler's optional map check has been exercised against the captured
baseline ELF/map and a deliberately mismatched compact-ELF/baseline-map pair.
It accepts the former and rejects the latter. The stale compact archive copy
was removed; the compact ELF, image hashes and linked-symbol measurements are
retained.

## Dependency replacements and upstream work

| Candidate | Assessment |
| --- | --- |
| SHA-2 compact backend | First measure the existing implementation switch. It avoids a crypto dependency migration. Check image size, stack floor and messaging latency. |
| `micropb` 0.7.0 | Worth a prototype with the full schema. It supports alloc or bounded containers, callbacks, cached nested lengths and separate encode/decode runtime features. It does not support closed enums, which prevents treating it as a drop-in replacement. No flash saving has been measured. |
| `prost` | A comparison candidate using the same schema and target. A migration must account for `MessageField`, views, size caching, pinned entry points, enum behavior and persistence. Its use of generic buffer traits does not by itself eliminate duplicate instantiations. No flash saving has been measured. |
| Dalek without precomputed tables | Implemented in upstream draft #1861. The table disappears from the target ELF. Combined target delta is 39,104 bytes; fallback code and a larger worker stack affect that comparison. Both native configurations pass 302 tests, plus focused Clippy. Complete compact QEMU two-device and four-peer persistence runs pass, but two initial watchdog failures remain open. Keep this backend opt-in. |
| Replace mbedTLS with rustls | Not justified by this profile. A second TLS stack may duplicate platform crypto and certificate parsing. Measure feature trimming first and preserve server authentication. |

The largest structural protobuf opportunities are shared field codec helpers,
reducing duplicate drop/clone code, and separating outgoing codecs while keeping
complete incoming decoding. The current upstream already centralizes codec
entry points and pins clones, so proposing those as new work would be stale.
A schema subset changes supported behavior; it cannot be presented as a
transparent reduction for the complete client.

References checked on 2026-11-05:

- [micropb README](https://github.com/YuhanLiin/micropb/blob/main/README.md),
  blob `142487a000c683f1d5439fc3f7e0dde7e176f3c3`.
- [prost README](https://github.com/tokio-rs/prost/blob/master/README.md),
  blob `725c31ca67562ac81d2095faf976e22d3ebf499d`.
- Upstream `waproto/build.rs`, `waproto/src/lib.rs` and
  `wacore/libsignal/Cargo.toml` at the pinned protocol revision.

## Reproduction and remaining qualification

```sh
python3 scripts/test-flash-layout.py
# Run from outside the firmware tree to avoid its target/build-std defaults:
# RUSTFLAGS='--cfg sha2_backend="soft" --cfg sha2_backend_soft="compact"' \
#   cargo test --locked --release --manifest-path /path/to/repo/tests/compact-sha/Cargo.toml
python3 scripts/check-flash-layout.py app.bin partitions.esp32c3.csv 4MB
scripts/profile-app.py path/to/whatsapp-esp32 \
  --nm path/to/riscv32-esp-elf-nm --image app.bin --map app.map > symbols.json
```

Use `esp_idf_size --archives` and `--files` on the resolved linker map for C
archive/object attribution. The profiler checks the map's `.flash.text` address
and size against the ELF and rejects a mismatch; its hash is recorded with the
ELF and image hashes. Do not sum discarded map input sections. ELF
filesystem size or stripping debug symbols is not a flashed-image measurement.

- [x] Measure the current production baseline and retain provenance.
- [x] Add the physical-flash regression gate and its five host tests.
- [x] Verify compact SHA-256/SHA-512 against known vectors on the host.
- [x] Finish the first compact production build and record the actual image delta.
- [x] Measure ESP-IDF C/C++ save/restore.
- [x] Finish shared-generic/path-trim measurements.
- [x] Choose a 4 MiB layout with explicit app margin and storage capacity.
- [x] Boot an actual 4 MiB QEMU image, pair, reboot with the store intact, and
  exchange messages, reactions, edits and quoted replies between two boards.
- [x] Check stack/heap floors and restored-session behavior.
- [ ] Resolve compact-backend interrupt watchdog failures before promoting it.
- [ ] Smoke-test production TLS and the selected layout on a physical C3.

The store's 2048-entry constants are logical limits, not measured NVS capacity.
Metadata, garbage collection and variable-sized records consume flash. Any
smaller store needs a populated workload and an explicit capacity limitation.
Changing the partition layout moves `wa_store`; existing devices need a backup
and migration or an explicit erase-and-pair procedure.
