# Fitting the complete C3 firmware into 4 MiB flash

This is an active investigation. The measured production baseline is **4,188,368
bytes**. The first combined compact build measures **3,819,600 bytes**, saving
**368,768 bytes / 8.8%**. Runtime qualification is still in progress; the branch
does not yet claim support for a physical 4 MiB C3.

## Physical budget

A 4 MiB device has 4,194,304 bytes. With the app starting at `0x10000`, a 64 KiB
core dump and the existing 1 MiB WhatsApp store, the application budget is
**3,014,656 bytes**, or `0x2e0000`. Getting the app below 4,000,000 bytes alone
does not make the board usable.

The existing table ends at `0x5e0000`, beyond a 4 MiB device, even if the app
itself were smaller. This branch adds a full-layout check to CI and QEMU image
generation, including partition alignment, overlap, physical flash bounds and
application fit. The existing board layouts remain selected until a smaller
image and a populated-store workload have been qualified.

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

The 3,819,600-byte image fits an app slot of `0x3b0000`, leaving 192 KiB for the
store and 64 KiB for the core dump, with 47,024 bytes of app margin. A 256 KiB
store requires the app to fit `0x3a0000`, another 18,512 bytes smaller. ESP-IDF's
documented `CONFIG_COMPILER_SAVE_RESTORE_LIBCALLS` option is now being measured
for the C/C++ side before choosing the final layout.

## Changes being measured

The current compiler configuration is experimental until both the production
image and the QEMU workload complete:

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

## Dependency replacements and upstream work

| Candidate | Assessment |
| --- | --- |
| SHA-2 compact backend | First measure the existing implementation switch. It avoids a crypto dependency migration. Check image size, stack floor and messaging latency. |
| `micropb` 0.7.0 | Worth a prototype with the full schema. It supports alloc or bounded containers, callbacks, cached nested lengths and separate encode/decode runtime features. It does not support closed enums, which prevents treating it as a drop-in replacement. No flash saving has been measured. |
| `prost` | A comparison candidate using the same schema and target. A migration must account for `MessageField`, views, size caching, pinned entry points, enum behavior and persistence. Its use of generic buffer traits does not by itself eliminate duplicate instantiations. No flash saving has been measured. |
| Dalek without precomputed tables | The linked table is 30,720 bytes, not the README's general ~400 KB estimate. Both the direct Dalek edge and `x25519-dalek` enable defaults upstream. Cargo features are additive, so a root dependency cannot switch them off. An upstream feature split must preserve alloc/zeroize and qualify slower basepoint multiplication, pairing, prekey generation and watchdog behavior. |
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
python3 scripts/check-flash-layout.py app.bin partitions.csv 16MB
scripts/profile-app.py path/to/whatsapp-esp32 \
  --nm path/to/riscv32-esp-elf-nm --image app.bin > symbols.json
```

Use `esp_idf_size --archives` and `--files` on the resolved linker map for C
archive/object attribution. Do not sum discarded map input sections. ELF
filesystem size or stripping debug symbols is not a flashed-image measurement.

- [x] Measure the current production baseline and retain provenance.
- [x] Add the physical-flash regression gate and its four host tests.
- [x] Verify compact SHA-256/SHA-512 against known vectors on the host.
- [x] Finish the first compact production build and record the actual image delta.
- [ ] Measure ESP-IDF C/C++ save/restore and select the final configuration.
- [ ] Choose a 4 MiB layout with explicit app margin and storage capacity.
- [ ] Boot an actual 4 MiB QEMU image, pair, reboot with the store intact, and
  exchange messages, reactions, edits and quoted replies between two boards.
- [ ] Check stack/heap floors and restored-session behavior.
- [ ] Smoke-test production TLS and the selected layout on a physical C3.

The store's 2048-entry constants are logical limits, not measured NVS capacity.
Metadata, garbage collection and variable-sized records consume flash. Any
smaller store needs a populated workload and an explicit capacity limitation.
Changing the partition layout moves `wa_store`; existing devices need a backup
and migration or an explicit erase-and-pair procedure.
