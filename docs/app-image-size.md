# Where the 4.1 MB app image goes

The firmware is large enough that flash size, not RAM, is what rules boards out:
`partitions.csv` gives the app a 0x4C0000 (4,980,736 byte) factory partition, the
image fills 82.6% of it, and no 4 MB board can hold it at all. This is a
measurement of what is actually in there, and of what the obvious levers are
really worth.

Nothing here is applied. Every number below was produced by building and
measuring; the recommendations at the end are proposals.

## Method

Measured on the **ESP32-C3 release build** (`scripts/build.sh --board esp32c3
--release --features mock-server`, from the port in
[#7](https://github.com/oxidezap/whatsapp-rust-esp32/pull/7)), because that is
the build whose image size was the open question. The ESP32-S3 image is larger
(~4.5 MB) but the composition is the same: the difference is instruction
encoding, not content.

Tools, all from the toolchain the build already installs under `.embuild`:

```bash
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

| # | Change | Image | Delta | What it costs |
| --- | --- | --- | --- | --- |
| **D** | `std` built without the `backtrace` feature (`-Zbuild-std-features=compiler-builtins-mem`) | 4,024,880 | **−87,792 (−2.1%)** | **Nothing.** Verified: zero `gimli::` symbols remain, and panic messages still come from the hook in `crash.rs`. |
| **A** | `log` with `release_max_level_info` | 4,029,216 | **−83,456 (−2.0%)** | All `debug!`/`trace!` call sites and their format strings, across every crate. The demo deliberately runs at DEBUG to show the protocol flow. |
| **B** | `CONFIG_MBEDTLS_CERTIFICATE_BUNDLE_DEFAULT_CMN=y` | 4,072,640 | **−40,032 (−1.0%)** | The full root bundle drops to the common CAs. Needs checking against the real gateway's chain before adoption; irrelevant to `mock-server` builds, which verify nothing. |
| **C** | `panic = "immediate-abort"` | 3,921,760 | **−190,912 (−4.6%)** | **Panic messages and locations, entirely.** Directly defeats `src/crash.rs`. Includes D's saving. |
| | **D + A + B** (keeps panic capture) | 3,879,360 | **−233,312 (−5.7%)** | debug logs + bundle scope |
| | A + B + C (loses panic capture) | 3,776,016 | **−336,656 (−8.2%)** | the above plus panic diagnostics |

Two things follow.

**D is free and should just be done.** 88 KB for a symbolizer that cannot
symbolize anything in this firmware. It is one line in `.cargo/config.toml`
(`build-std-features`), and it is worth more than the certificate bundle.

**No combination of build switches gets this under 4 MB.** Everything above,
including the option that throws away panic diagnostics, lands at 3.78 MB. The
8 MB flash floor is structural.

## Where the remaining mass is, and who owns it

The measured levers are worth ~8% between them. `waproto` alone is ~15%, and
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
- Each experiment was built and imaged, **not booted**. They are size
  measurements. D is the only one recommended for adoption, and it should be
  booted on QEMU before it is.
- B changes which CAs the device trusts. That is a security-relevant change and
  needs a real-gateway handshake, which neither QEMU nor the mock server can
  provide.
