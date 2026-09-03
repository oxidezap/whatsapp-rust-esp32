# Where the app image goes

The firmware is large enough that flash size, not RAM, is what rules boards out:
`partitions.csv` gives the app a 0x4C0000 (4,980,736 byte) factory partition, and
no 4 MB board can hold the image at all. This is a measurement of what is
actually in there, and of what the obvious levers are really worth.

The image measured throughout is the **4,112,672-byte baseline** this analysis
started from. Three of the levers found here are now applied as defaults (A, B
and D), so the tree currently builds **3,879,360 bytes -- 233,312 fewer, 5.7%**,
and the factory partition went from 82.6% to 77.9% full. The composition tables
below are the baseline's; the levers and what they cost are at the end. Every
number was produced by building and measuring, not estimated.

## Method

Measured on the **ESP32-C3 release build**, because that is the build whose image
size was the open question. The ESP32-S3 image is larger (~4.5 MB) but the
composition is the same: the difference is instruction encoding, not content, and
all three applied levers are chip-independent.

```bash
scripts/build.sh --board esp32c3 --release --features mock-server
```

Substitute any board in `scripts/boards.sh` and the rest of this section follows;
`board_out_dir` in that file is what turns a board name into the paths below.

Tools, all from the toolchain the build already installs under `.embuild`:

```bash
# Or: source scripts/boards.sh; board_select esp32c3; OUT=$(board_out_dir)
OUT=target/riscv32imc-esp-espidf/release
BIN=.embuild/tools/riscv32-esp-elf/*/riscv32-esp-elf/bin
MAP=$OUT/build/esp-idf-sys/*/out/build/libespidf.map
IDFPY=.embuild/python_env/idf5.5_py3.11_env/bin/python

$BIN/riscv32-esp-elf-size -A $OUT/whatsapp-esp32          # sections
$IDFPY -m esp_idf_size --archives $MAP                     # per static library
$IDFPY -m esp_idf_size --files $MAP                        # per object file
$BIN/riscv32-esp-elf-nm --print-size --size-sort -r -C $OUT/whatsapp-esp32
```

One caution learned the hard way: parsing the linker map by hand over-counts
badly, because the map lists input sections that `--gc-sections` then discards.
An early pass of this analysis "found" ~200 KB of ESP-WIFI-MESH in the image;
`esp_idf_size` puts `libmesh.a` at **60 bytes**. Use `esp_idf_size`, which reads
the map the way the linker resolved it.

## The top-level split

```
.flash.text     3,429,156   83.4%
.flash.rodata     598,836   14.6%
.iram0.text        71,606
.dram0.data        12,416
                ---------
image total     4,112,672
```

Per object file, everything above 10 KB of flash:

| Object | Flash | Share |
| --- | --- | --- |
| **the Rust code (one LTO codegen unit)** | **3,337,950** | **81.2%** |
| `x509_crt_bundle.S.obj` | 68,987 | 1.7% |
| `mdns.c.obj` | 25,730 | 0.6% |
| Wi-Fi driver (`pm`, `ieee80211_*`, `pp`, `wl_cnx`, `wdev`, `lmac`, …) | ~130,000 | 3.2% |
| `http_parser.c.obj` | 11,452 | 0.3% |
| mbedTLS (`ssl_tls`, `ecp`, `psa_crypto`, …) | ~100,000 | 2.4% |
| everything else in ESP-IDF | ~440,000 | 10.7% |

**ESP-IDF is not the problem.** Four fifths of the image is our own Rust, in a
single LTO'd codegen unit, and that is where any serious reduction has to come
from.

## Inside the Rust code

Symbol sizes aggregated by crate (3,652,594 bytes of named symbols; the rest of
`.rodata` is anonymous literals, see below):

| Crate | Bytes | Share of named symbols |
| --- | --- | --- |
| ESP-IDF C and assembly | 789,443 | 21.6% |
| `whatsapp_rust` | 760,000 | 20.8% |
| `waproto` | 488,380 | 13.4% |
| `core` | 400,588 | 11.0% |
| `wacore` | 210,209 | 5.8% |
| `whatsapp_esp32` (this crate) | 117,488 | 3.2% |
| `alloc` | 96,546 | 2.6% |
| `wacore_binary` | 93,974 | 2.6% |
| `std` | 93,828 | 2.6% |
| `wacore_libsignal` | 71,378 | 2.0% |
| `curve25519_dalek` | 55,882 | 1.5% |
| `sha2` | 48,550 | 1.3% |
| `hashbrown` | 42,472 | 1.2% |
| `serde_json` | 37,614 | 1.0% |

This crate is 3.2%. The protocol stack is the firmware.

### `waproto`: 610 KB of generated protobuf

Counting generic instantiations (`<waproto::X as buffa::Message>::...`), the
generated code totals **609,900 bytes across 164 message types** — about 15% of
the whole image. It is dominated by one type:

| Generated type | Bytes |
| --- | --- |
| `Message` (the top-level oneof) | 202,898 |
| `message` (its submodule of variants) | 138,236 |
| `BotMetadata` | 51,730 |
| `SyncActionValue` | 41,670 |
| `ContextInfo` | 40,312 |
| `WebMessageInfo` | 15,508 |
| `MessageContextInfo` | 12,404 |

`Message` and its variants together are **341,134 bytes, 8.3% of the image**, and
the four largest single symbols in the entire binary are its codecs:

```
 66,812  <waproto::whatsapp::Message as buffa::Message>::merge_field::<&[u8]>
 46,778  <waproto::whatsapp::Message as buffa::Message>::compute_size
 44,100  <waproto::whatsapp::Message as buffa::Message>::write_to::<Vec<u8>>
 21,782  <waproto::whatsapp::Message as core::clone::Clone>::clone
```

That is the shape of a protobuf oneof with hundreds of variants: every variant
contributes to one giant match in each of encode, decode, size and clone, and the
linker cannot drop a variant because the match arm is reachable. A demo bot that
sends text, a reaction and an edit pays for every message type WhatsApp has ever
defined, `BotMetadata` and `AIRichResponseSubMessage` included.

### `whatsapp_rust`: 760 KB, mostly one module

| Module | Bytes |
| --- | --- |
| `client` | 635,550 |
| `handlers` | 123,060 |
| `portable_cache` | 35,248 |
| `features` | 24,270 |
| `store` | 19,046 |
| `history_sync` | 16,474 |
| `download` | 7,056 |

### 128 KB of backtrace symbolizer that can never run

| Crate | Bytes |
| --- | --- |
| `gimli` (DWARF parser) | 78,872 |
| `std::sys` backtrace / symbolize glue | 22,821 |
| `rustc_demangle` | 12,698 |
| `zlib_rs` (only reachable from `gimli`) | 11,934 |
| `object`, `addr2line` | 1,890 |
| **total** | **128,215 (3.1% of the image)** |

This is a full DWARF-parsing backtrace symbolizer, linked into a firmware that

- sets `panic = "abort"` in both profiles,
- reboots on panic,
- and captures panics through a `std::panic::set_hook` that records the message
  and location to RTC RAM (`src/crash.rs`), plus the ESP-IDF core dump — never
  through `std::backtrace`.

The comment on `[profile.dev]` in `Cargo.toml` says matching `panic = "abort"`
lets "LTO GC unwind landing pads and the dead backtrace/gimli symbolizer". It
gets the landing pads. It does not get the symbolizer: `build-std` compiles `std`
with its default features, which include `backtrace`, and `std`'s panic runtime
references it unconditionally.

### Strings: 274 KB

6,792 printable runs of 8+ characters, 273,899 bytes (6.7% of the image):

| Kind | Bytes |
| --- | --- |
| log and panic messages | 149,330 |
| source paths (`.rs`, `/.cargo/`) | 37,888 |
| protobuf/JSON field names | 11,383 |
| dashboard HTML/CSS/JS | 7,286 |

The largest single literals are the WhatsApp binary-XMPP token dictionaries in
`wacore_binary` (`TOK_KEYS` 16,384, `TOK_BLOB` 9,080, `TOK_META` 8,192, `TOK_OFF`
8,192 = 41,848 bytes). Those are protocol data and cannot go. Next is
`curve25519_dalek`'s `ED25519_BASEPOINT_TABLE` at 30,720 bytes, which is a
speed/size tradeoff owned upstream.

## What the levers are actually worth

Each was built and imaged; each row is a real image size, not an estimate.
Baseline **4,112,672**.

| # | Change | Image | Delta | What it costs | |
| --- | --- | --- | --- | --- | --- |
| **D** | `std` built without the `backtrace` feature (`build-std-features` in `.cargo/config.toml`) | 4,024,880 | **−87,792 (−2.1%)** | **Nothing.** Verified: zero `gimli::` symbols remain, and panic capture is unaffected. | **applied** |
| **A** | `log` with `release_max_level_info` | 4,029,216 | **−83,456 (−2.0%)** | All `debug!`/`trace!` call sites and their format strings, in **release** builds. A debug build still traces the protocol flow. | **applied** |
| **B** | `CONFIG_MBEDTLS_CERTIFICATE_BUNDLE_DEFAULT_CMN=y` | 4,072,640 | **−40,032 (−1.0%)** | The full root bundle drops to 43 common CAs. Production builds only. | **applied** |
| **C** | `panic = "immediate-abort"` | 3,921,760 | **−190,912 (−4.6%)** | **Panic messages and locations, entirely.** Defeats `src/crash.rs`. | not applied |
| | **A + B + D**, what this tree now builds | **3,879,360** | **−233,312 (−5.7%)** | | |
| | A + B + C | 3,776,016 | −336,656 (−8.2%) | the above plus panic diagnostics | |

### Why A, B and D and not C

**D costs nothing at all.** 88 KB for a symbolizer that cannot symbolize anything
in this firmware: it aborts on panic, reboots, and reports through the hook in
`src/crash.rs` and the ESP-IDF core dump. Naming a `build-std-features` list is
what drops it, because that overrides `std`'s defaults, of which `backtrace` is
one.

**A only affects release builds.** `release_max_level_info` leaves `cargo build`
without `--release` fully verbose, so the protocol-level tracing the demo is
built to show is one flag away rather than gone. Every marker the QEMU
end-to-end suite waits on is `info!`, checked before applying: `whatsapp-esp32
starting`, `Ethernet connected! IP:`, `WhatsApp NVS loaded`, `QR CODE`,
`Connected to WhatsApp!`, `Bot built, starting run loop`, `Reaction sent`, `Send
took`.

**B is the one with a real caveat.** It narrows which roots the device will
trust. The common set is 43 CAs including 11 DigiCert roots -- the family Meta
issues from -- plus Amazon, GlobalSign, Google Trust Services, ISRG, Sectigo,
GoDaddy and IdenTrust. It could not be verified against the live gateway from
here, and neither CI nor the QEMU suite covers it, because a `mock-server` build
configures no CA at all. The failure mode if the chain ever falls outside the set
is a loud TLS handshake failure at connect, not a silent downgrade, and the fix
is one line. **A production build should still be smoke-tested against the real
gateway once.**

**C is refused.** 191 KB is the largest single saving available, and it is the
only one that would delete a feature this firmware was deliberately built around:
`crash.rs` exists to capture the panic message and location into RTC RAM so the
*next* boot can report why the last one died. Trading that for 4.6% of flash on a
device with no console attached is the wrong way round.

**And none of it changes the shape of the problem.** The smallest image measured
here, A+B+C at 3,776,016 bytes, is itself under 4 MB -- but the board is not the
image. A 4 MB part has 4,194,304 bytes for the bootloader, the partition table,
the `nvs` partition, the app, the 64 KB core dump *and* the 1 MB `wa_store`. At
3.78 MB of app that is over by more than a megabyte, and re-cutting
`partitions.csv` does not close it: even with the core dump gone and the store
down to 256 KB the total is still past 4 MB. The 8 MB flash floor is structural,
and no build switch reaches it.

## Where the remaining mass is, and who owns it

The measured levers are worth ~8% between them, and 5.7% is now taken. `waproto` alone is ~15%, and
`whatsapp_rust::client` another ~15%. Both are upstream in
[`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), and that is where a
step change would have to come from:

1. **Feature-gate the generated protobuf schema.** If `waproto` could generate
   only the message variants a consumer asks for, the 341 KB `Message` family is
   where the first 200+ KB would come from. This is the single largest item in
   the image and the one with the clearest structural fix.
2. **Split `Message`'s codecs by direction.** A device that never *sends* a
   `PollCreationMessage` still links `write_to` and `compute_size` for it. Encode
   and decode paths could be gated separately.
3. **Drop `Clone` on the big generated types** (21,782 bytes for `Message`
   alone) where the protocol engine can borrow instead.
4. **`portable_cache` (35 KB) and `features` (24 KB)** look like candidates for
   a smaller-footprint mode on constrained targets.

None of that belongs in this repository, but all of it is measurable from here:
rebuild, re-run the four commands under **Method**, and the tables above are
directly comparable.

## Caveats

- Sizes are from the ESP32-C3 build; the S3 image is ~10% larger with the same
  composition.
- The applied combination (A+B+D) was **booted on QEMU**, not only imaged: it
  reaches `WebSocket connected` with every end-to-end marker present, and the
  smaller image also returns RAM -- free heap at the `Free heap:` line goes from
  173,880 to 196,392 bytes, and the all-time low under load from 57,016 to
  59,088. Experiment C was imaged only; it is not applied.
- B changes which CAs the device trusts. That is security-relevant, and it needs
  a real-gateway handshake that neither QEMU nor the mock server can provide, so
  it is the one item here still owed a check on hardware.
