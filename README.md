# whatsapp-esp32

> **How far down the hardware ladder can a real WhatsApp client go?**
>
> We maintain [Baileys](https://github.com/WhiskeySockets/Baileys) and we wrote
> [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), so the WhatsApp
> protocol is kind of our thing. One day we got curious about how small the
> hardware running it could actually get. This is what came out, and honestly it
> went a lot further than we expected.
>
> It's a full, end-to-end-encrypted WhatsApp client running on an **ESP32-S3**: a
> **240 MHz** microcontroller with **512 KB of internal SRAM** (only a few tens of
> KB of it actually free at runtime) and **8 MB of external PSRAM**. That's the
> kind of chip you'd normally use to blink an LED or read a temperature sensor.
>
> And it genuinely works. It pairs over a QR code like any other linked device. It
> runs the full Noise handshake and Signal double-ratchet crypto in software, with
> no hardware AES. It sends and receives messages, reacts, edits, and serves a live
> status dashboard over HTTP. All of that on a chip with thousands of times less
> RAM and a small fraction of the clock speed a phone takes for granted.

A WhatsApp client running on an **ESP32-S3** microcontroller, built on top of
[`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust).

It pairs over QR code, connects to WhatsApp (or a local mock server) over an
encrypted WebSocket, runs a small demo bot, and serves a status dashboard over
HTTP. See [Pair and use](#pair-and-use) for the bot behavior and dashboard.

> **Heads up:** this is a demonstration and research project, not a product, and it
> is not affiliated with, endorsed by, or connected to WhatsApp or Meta in any way.
> Running an unofficial client against the real WhatsApp service can break their Terms
> of Service and may get the number banned, so test with a spare number and the local
> mock server. A few flags here are deliberately insecure for that local setup
> (`SKIP_TLS_VERIFY`, the `danger-skip-cert-chain-verify` feature, and `MOCK_AUTOPAIR`);
> turn them off before pointing this at the real gateway.

## Hardware

- **Board:** ESP32-S3 N16R8 (16 MB flash, 8 MB octal SPI PSRAM).
- The PSRAM is required: the main async task runs on a 256 KB stack allocated
  from PSRAM, which is far larger than internal SRAM can provide.

## How it works

`whatsapp-rust` is platform-agnostic. This crate provides the ESP32 implementations
of its runtime traits:

| File | Provides | Role |
|------|----------|------|
| `src/storage.rs` | `MemoryStore` | In-memory `Backend` (Signal / AppSync / Protocol / MsgSecret / Device stores). State is lost on reboot. |
| `src/transport.rs` | `Esp32TransportFactory` | ESP-IDF mbedTLS + `tungstenite` WebSocket, driven on a dedicated `std::thread`. |
| `src/http_client.rs` | `EspHttpClient` | Raw HTTP/1.1 over ESP-IDF TLS/TCP (used for media + version fetch). |
| `src/runtime.rs` | `Esp32Runtime` | `edge-executor`-based async runtime (spawn / sleep / yield). |
| `src/admin.rs` | `start_admin_server` | HTTP admin dashboard on port 8081. |
| `src/main.rs` | firmware entry | WiFi + SNTP + mDNS bringup, executor loop, the ping/pong bot. |

The protocol crates are pulled straight from git, with `default-features = false`
so the desktop-only features (tokio, SQLite, moka, SIMD) are dropped:

```toml
whatsapp-rust = { git = "https://github.com/oxidezap/whatsapp-rust", branch = "main", default-features = false }
wacore        = { git = "https://github.com/oxidezap/whatsapp-rust", branch = "main", default-features = false }
waproto       = { git = "https://github.com/oxidezap/whatsapp-rust", branch = "main" }
```

## Prerequisites

- The Espressif Rust toolchain: `cargo install espup && espup install`
  (`rust-toolchain.toml` pins `channel = "esp"`).
- `cargo install ldproxy` (the linker wrapper referenced by `.cargo/config.toml`).
- `cargo install espflash` for flashing and monitoring.
- ESP-IDF **v5.4** is downloaded and built automatically by `esp-idf-sys` on the
  first `cargo build` (into `.embuild/`, a few GB; subsequent builds reuse it).
- Host tools the ESP-IDF build needs: `git`, `python3`, `cmake`, `ninja`, `clang`.

## Configure

Copy the template and fill it in (the project still builds without a `.env`, but
won't connect until WiFi is set):

```bash
cp .env.example .env
```

```dotenv
WIFI_SSID=your-ssid          # 2.4 GHz only
WIFI_PASS=your-password
WHATSAPP_WS_URL=wss://192.168.0.4:8080/ws/chat   # optional; mock or gateway
```

These are read at build time and baked into the firmware, so changing them needs
a rebuild + reflash.

TLS verification is a source constant in `src/main.rs`:

```rust
const SKIP_TLS_VERIFY: bool = true; // skip server cert verification (mock server)
```

For a local mock server keep `SKIP_TLS_VERIFY = true`: the firmware then connects with
**no** CA configured, so esp-tls applies `MBEDTLS_SSL_VERIFY_NONE` and accepts whatever
certificate the server presents. This is required because the mock server (`barback`)
mints a fresh ephemeral self-signed cert on every start, so there is nothing stable to
pin against. It relies on `CONFIG_ESP_TLS_INSECURE=y` +
`CONFIG_ESP_TLS_SKIP_SERVER_CERT_VERIFY=y` in `sdkconfig.defaults`. For the real
WhatsApp gateway, point `WHATSAPP_WS_URL` at it and set `SKIP_TLS_VERIFY = false` so the
ESP-IDF root certificate bundle is used (verification enforced).

## Build

```bash
cargo build              # debug (opt-level "z", fat LTO, ~14 MB ELF with debug info)
cargo build --release    # release
```

The target (`xtensa-esp32s3-espidf`), `build-std`, and the `MCU` / `ESP_IDF_VERSION`
environment variables all come from `.cargo/config.toml`, so a bare `cargo build`
is enough.

## Flash and monitor

Install `cargo install espflash`. On Arch your user must be in the `uucp` group to
access the serial port (`dialout` on Debian/Ubuntu).

The ESP32-S3 exposes a built-in USB-Serial/JTAG (USB id `303a:1001`), so it shows up
directly as `/dev/ttyACM0`, with no external UART adapter needed. Confirm the link first:

```bash
espflash board-info --port /dev/ttyACM0   # prints chip type, flash size, MAC
```

Flash the firmware. You must pass the ESP-IDF bootloader and **our** custom
partition table explicitly. Without `--partition-table`, espflash falls back to a
1.5 MB factory partition that the ~4.3 MB app does not fit:

```bash
espflash flash \
  --port /dev/ttyACM0 \
  --bootloader target/xtensa-esp32s3-espidf/debug/bootloader.bin \
  --partition-table partitions.csv \
  target/xtensa-esp32s3-espidf/debug/whatsapp-esp32
```

`espflash` takes the ELF directly and converts it on the fly. The `dev` profile
already builds at `opt-level = "z"`, so that path is the normal flow (use
`release/` if you ran `cargo build --release`). The bootloader/partition-table
binaries are produced by the ESP-IDF build under `target/.../debug/`. A successful
flash prints `App/part. size 4,503,680/4,980,736 (90.42%)`. The second number is
our 4864K partition, confirming it is in use.

Useful flags: `--baud 921600` to flash faster, `--monitor` to drop into the serial
console afterwards.

### Watching the serial log

In an interactive terminal, just append `--monitor` to the flash command (or run
`espflash monitor --port /dev/ttyACM0 --elf target/.../whatsapp-esp32`; the `--elf`
lets it symbolize panic backtraces). `CTRL+R` resets the chip, `CTRL+C` exits.

Headless/scripted, `espflash monitor` is awkward (it wants to sync with the
bootloader). To capture a fresh boot non-interactively, reset and read the raw
device (the USB-CDC port ignores baud):

```bash
espflash reset --port /dev/ttyACM0          # restart the app
timeout 20 cat /dev/ttyACM0                  # capture boot: WiFi, IP, admin URL, heap
```

A healthy boot ends with `WiFi connected! IP: <ip>`, the admin server starting on
port 8081, and `Bot built, starting run loop`. With no server reachable at the
configured URL you'll see TLS connect failures and an exponential reconnect backoff
(`935ms → 1.056s → …`). That's expected, and a quick way to confirm the device is
alive and its async timers work.

## Pair and use

1. After flashing, watch the serial log (or the dashboard) for the QR code.
2. On your phone: WhatsApp > Linked Devices > Link a Device, and scan it.
3. Open the dashboard at `http://esp32-whatsapp.local:8081/dashboard`
   (mDNS) or `http://<device-ip>:8081/dashboard`. It renders the QR, shows the
   paired PN/LID, free heap, and session/identity/prekey counts, and exposes
   Clear Sessions / Factory Reset / Reboot actions.
4. From a linked chat, send `🦀ping`. The device reacts with 🏓, replies
   `🏓 Pong!` quoting your message, then edits the reply with the measured send
   latency.

## Admin endpoints

The dashboard is backed by a small HTTP API on port 8081 (the dashboard page
itself pulls `qrcode.min.js` from a CDN, so the browser needs internet access):

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/dashboard` | The HTML dashboard. |
| GET | `/` | JSON store stats (heap, sessions, identities, prekeys, paired). |
| GET | `/device` | Pairing status: QR code, connection, PN/LID. |
| GET | `/metrics` | Live system telemetry (see Diagnostics below). |
| GET | `/health` | Liveness check (`ok`). |
| GET | `/sessions` | List Signal session addresses. |
| DELETE | `/sessions` | Clear all Signal sessions. |
| POST | `/reset` | Factory reset (wipe all in-memory state). |
| POST | `/reboot` | Reboot the device. |
| POST | `/test-panic` | Deliberately panic, to exercise the persistent crash capture (see Diagnostics). |

Handy for a quick liveness check from the same network, using the IP from the boot
log (mDNS `.local` resolution is unreliable across some routers / 2.4-vs-5 GHz SSIDs):

```bash
curl http://<device-ip>:8081/         # {"status":"running","heap_free":...}
curl http://<device-ip>:8081/device   # {"connected":...,"qr_code":...}
curl -X POST http://<device-ip>:8081/reboot
```

## Diagnostics & telemetry

The dashboard and `GET /metrics` expose live numbers read straight from ESP-IDF
(nothing is estimated):

```bash
curl http://<device-ip>:8081/metrics
# {"reset_reason":"PowerOn","uptime_s":120,"heap_free":3493328,
#  "heap_internal_free":29775,"heap_min_free":3299632,
#  "internal_largest_block":7680,"internal_min_free":4755,
#  "psram_free":3463948,"rssi_dbm":-65}
```

`internal_*` is the scarce resource on this board: **internal DRAM** (~tens of KB,
DMA-capable), separate from the 4 MB PSRAM. `internal_min_free` is the all-time
low-water mark. Watch it, since the AES/TLS/prekey paths compete for internal
DRAM and that's what OOMs first.

**Crash cause is captured, not guessed:**
- A panic hook logs the real Rust panic with its source location and message,
  e.g. `RUST PANIC: panicked at src/admin.rs:NN: <message>`, before the abort.
- `reset_reason` (also logged at boot as `last reset: ...`) tells you why the
  *previous* run ended: `Panic`, `TaskWatchdog`, `Brownout`, `PowerOn`, etc.
- Hardware exceptions (LoadProhibited, …) print a `Backtrace: 0x... 0x...` of PCs.
  Symbolize it with the monitor (`espflash monitor --elf target/.../whatsapp-esp32`)
  or directly:

  ```bash
  xtensa-esp32s3-elf-addr2line -fCe target/xtensa-esp32s3-espidf/debug/whatsapp-esp32 0x42002fe2 0x...
  ```

## Troubleshooting

- **`Stack canary watchpoint triggered` / stack overflow:** bump
  `MAIN_TASK_STACK_SIZE` in `src/main.rs` (the full send path with a quoted reply
  and edit is stack-heavy).
- **TLS handshake fails against the mock server (`mbedtls_ssl_handshake returned
  -0x2700` / "Failed to verify peer certificate"):** the mock server regenerates its
  self-signed cert on every start, so verification cannot succeed against any pinned CA.
  Ensure `SKIP_TLS_VERIFY = true` in `src/main.rs` **and** that
  `CONFIG_ESP_TLS_INSECURE=y` + `CONFIG_ESP_TLS_SKIP_SERVER_CERT_VERIFY=y` are in
  `sdkconfig.defaults`, then rebuild + reflash. With those set the firmware skips cert
  verification entirely and the regenerated ephemeral cert no longer matters.
- **`AtomicU64` / 64-bit atomic link errors from a dependency:** Xtensa has no
  native 64-bit atomics; dependencies must use `portable_atomic`. The project
  relies on `portable-atomic` with the `fallback` feature for this.
- **ESP-IDF build fails on very new host toolchains:** ESP-IDF v5.4 is happiest
  with cmake < 4 and Python <= 3.12. If your host ships newer ones, the
  `esp-idf-sys` bootstrap may complain.

## License

MIT, the same license as [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust).
See [LICENSE](LICENSE).
