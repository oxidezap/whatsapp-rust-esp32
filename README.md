# whatsapp-esp32

A real, end-to-end-encrypted WhatsApp client on an ESP32. We maintain
[Baileys](https://github.com/WhiskeySockets/Baileys) and wrote
[`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), and at some point
we started wondering how small a chip could still run the whole thing. Turns out
the answer is a 240 MHz microcontroller with a few hundred KB of RAM. It pairs
over QR like any linked device, does the Noise handshake and the Signal
double ratchet in software, sends and receives messages, and serves a status
dashboard over HTTP.

It runs on the ESP32-S3, the ESP32-C5 and the ESP32-C3, the last one with no
PSRAM at all. Pairing and Signal state live in flash, so a reboot comes back as
the same linked device.

> Not a product, and not affiliated with WhatsApp or Meta in any way. An
> unofficial client can get the number banned, so test with a spare number and
> the local mock server. The `mock-server` cargo feature is deliberately
> insecure (any TLS cert accepted, Noise cert check skipped, QR auto-scanned).
> It is off by default. A build without it is the one you point at the real
> gateway.

## Hardware

| Board | Chip | Flash / PSRAM | ESP-IDF | Build |
|-------|------|---------------|---------|-------|
| ESP32-S3 N16R8 devkit | Xtensa LX7, dual core | 16 MB / 8 MB octal | v5.5.5 | `cargo build --release --features mock-server` |
| [Waveshare ESP32-C5-Touch-LCD-2.8](https://github.com/waveshareteam/ESP32-C5-Touch-LCD-2.8) N16R8 | RISC-V, single core | 16 MB / 8 MB quad | v5.5.5 | `scripts/build.sh --board esp32c5 --release --features mock-server` |
| ESP32-C3, 16 MB flash | RISC-V, single core | 16 MB / none | v5.5.5 | `scripts/build.sh --board esp32c3 --release --features mock-server` |

Same source and same ESP-IDF on all three. A board is one row in
[`scripts/boards.sh`](scripts/boards.sh) plus one `sdkconfig.defaults.<chip>`
overlay. That table drives `scripts/build.sh`, `scripts/qemu.sh` and CI. A board
only gets built or emulated in CI once it also has a row in the `build` or
`qemu-e2e` matrix in [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

On the S3 and C5 the Rust heap, the 256 KB executor stack and every worker stack
but one live in 8 MB of PSRAM. The exception is `wa-nvs`, which stays in
internal DRAM on every board because writing flash disables the cache. The C3
has no PSRAM, so its ~400 KB of on-chip SRAM is the whole memory system
(about 314 KB reaches the heap) and the firmware sizes everything from
`CONFIG_SPIRAM`. Nothing in `src/` checks which chip it is. The full story is in
[docs/esp32c3.md](docs/esp32c3.md).

Flash is the hard floor. The app is ~4.2 MB and `partitions.csv` reserves 1 MB
for the store, so a board needs at least 8 MB and the table assumes 16 MB. The
common 4 MB C3 devkits do not fit. For what else could run this and on which
emulator, see [docs/board-support-map.md](docs/board-support-map.md).

## How it works

`whatsapp-rust` does not know about hardware. It defines four traits and the
platform fills them in. This repo is one crate with two targets. The library is
the ESP32 platform, the binary is a demo firmware on top of it.

| Module | Provides | Contract |
|--------|----------|----------|
| `storage` | `NvsStore` | `Backend`. Device, Signal state and sync keys live in the `wa_store` NVS partition and survive reboots. The rest is a RAM cache. |
| `transport` | `Esp32TransportFactory` | `TransportFactory`. ESP-IDF mbedTLS under the crate's own single-owner WebSocket client (`ws`), on its own thread. |
| `http_client` | `EspHttpClient` | `HttpClient`. Streaming HTTP/1.1 over ESP-IDF TLS/TCP with bounded RAM. |
| `runtime` | `Esp32Runtime`, `Esp32Executor`, `BlockingWorker` | `Runtime`. An `edge-executor` loop that parks when idle. `spawn_blocking` runs on a dedicated thread so key generation never stalls the loop. |
| `psram_alloc` | `PsramAllocator` | Optional global allocator that keeps the Rust heap in PSRAM. |
| `supervisor` | `DeviceStatus`, `ActiveClient`, `MaintenanceCoordinator` | Bookkeeping. Which client is live, what the dashboard shows, the one path that erases or reboots. |
| `metrics`, `crash` | telemetry, panic and core-dump capture | What `/metrics` reports. |
| `admin` (default on) | `start_admin_server` | The dashboard and API. |
| `src/main.rs` | demo firmware | WiFi + SNTP + mDNS, the executor thread, a supervisor that rebuilds the bot when it exits, the ping/pong bot. |

## Using it as a library

No wrapper around `Bot` and no ESP32 event type. You write the same
`Bot::builder()` code as on desktop, plug in the four platform values, and read
`whatsapp-rust` docs for the rest.

```toml
[dependencies]
# Both from git, at the same whatsapp-rust revision this crate's Cargo.toml
# names. Two different whatsapp_rust copies in one build would hand the Bot
# builder trait objects the platform types do not implement.
whatsapp-rust = { git = "https://github.com/oxidezap/whatsapp-rust", rev = "e14300d18da6b0446e77b4777027b6ebbe995c03", default-features = false }
whatsapp-esp32 = { git = "https://github.com/oxidezap/whatsapp-rust-esp32", default-features = false }
esp-idf-svc = { version = "0.52", features = ["binstart", "critical-section"] }
anyhow = "1"
log = "0.4"
```

`examples/minimal.rs`, compiled by CI so it cannot rot (this is a copy):

```rust
//! The smallest firmware on the library: the same `Bot::builder()` code as on a
//! desktop, with the four ESP32 platform values plugged in.
//!
//! Compiled by CI (`cargo check --release --examples`) so it cannot rot; it does
//! not bring the network up, so it is not a runnable image. `src/main.rs` is the
//! complete firmware (WiFi, SNTP, supervisor, dashboard).

use std::sync::Arc;

use whatsapp_esp32::runtime::spawn_thread;
use whatsapp_esp32::{Esp32Executor, Esp32Runtime, Esp32TransportFactory, EspHttpClient, NvsStore};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::prelude::MessageExt as _;

// The Rust heap goes to PSRAM; internal DRAM stays free for FreeRTOS and mbedTLS.
// Only on a build that has PSRAM: on the ESP32-C3 there is one heap and it is
// internal DRAM, so the plain ESP-IDF allocator is the right one. `esp_idf_spiram`
// is the cfg esp-idf-sys derives from CONFIG_SPIRAM.
#[cfg(esp_idf_spiram)]
#[global_allocator]
static ALLOCATOR: whatsapp_esp32::psram_alloc::PsramAllocator =
    whatsapp_esp32::psram_alloc::PsramAllocator;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Bring WiFi and SNTP up here: the Noise handshake needs a roughly correct
    // clock, and the socket needs a route. See `bring_up_wifi` in src/main.rs.

    // Flash-backed: needs a `wa_store` NVS partition (see partitions.csv).
    let store = Arc::new(NvsStore::open_default()?);
    // One runtime, one executor. The runtime is cheap to clone; clone it per Bot.
    let (runtime, executor) = Esp32Runtime::create_default()?;

    // The executor needs a large stack (the send path has deep frames):
    // `default_thread_config` is 256 KB of PSRAM, or 32 KB of internal DRAM on a
    // chip without any.
    let main_thread = spawn_thread(&Esp32Executor::default_thread_config(), move || {
        executor.block_on(async move {
            let bot = Bot::builder()
                .with_backend_arc(store)
                .with_transport_factory(Esp32TransportFactory::default())
                .with_http_client(EspHttpClient::default())
                .with_runtime(runtime.clone())
                // 812 X25519 keypairs at login (the default) exhaust internal DRAM.
                .with_wanted_pre_key_count(50)
                // History is not persisted here, and its sync churns ~14 MB.
                .skip_history_sync()
                .on_qr_code(|code, timeout| async move {
                    log::info!("scan this QR (valid for {timeout:?}): {code}");
                })
                .on_message(|ctx| async move {
                    if ctx.message.text_content() == Some("ping") {
                        if let Err(error) = ctx.reply("pong").await {
                            log::error!("reply failed: {error}");
                        }
                    }
                })
                .build()
                .await;
            match bot {
                // Runs until logout or `Client::disconnect`; a firmware normally
                // loops here and rebuilds the bot (see `run_whatsapp` in src/main.rs).
                Ok(bot) => bot.run().await,
                Err(error) => log::error!("could not build the bot: {error}"),
            }
        })
    })?;
    main_thread
        .join()
        .map_err(|_| anyhow::anyhow!("executor thread panicked"))
}
```

`::default()` on the transport and HTTP client targets the production gateway
with certificates verified. `::new(url, skip_tls_verify)` is for a local mock
server. Thread stacks, priorities and cores can be tuned through
`Esp32TransportFactory::with_thread_config` and `BlockingWorker::start_with`.

The firmware around it provides four things (`src/main.rs`,
`sdkconfig.defaults` and `partitions.csv` show all of them working). PSRAM for
`malloc` and task stacks plus a 256 KB executor stack. A `wa_store` NVS
partition, 1 MB here. mbedTLS allocating from external memory, and the task
watchdog leaving the executor's idle task alone since the blocking worker runs
below idle priority. And time, because the Noise handshake needs a roughly
correct clock, so SNTP starts before the first connect.

One upstream dependency is enough since `whatsapp-rust` 0.7.0. It re-exports
`wacore`, `waproto`, `buffa` and the shared crates, so they can never drift out
of sync. Pin the same git revision this crate names, with
`default-features = false` to drop the desktop-only pieces (tokio, SQLite,
ureq, SIMD). `anyhow` and `futures` stay direct because this crate needs feature
flags on them that `whatsapp-rust` does not enable. Needs Rust 1.94 or newer.

## Prerequisites

- `cargo install espup && espup install` (`rust-toolchain.toml` pins the `esp` channel).
- `cargo install ldproxy` and `cargo install espflash`.
- ESP-IDF v5.5.5, fetched and built by `esp-idf-sys` on the first build into
  `.embuild/`. A few GB, reused afterwards.
- Host tools: `git`, `python3`, `cmake`, `ninja`, `clang`.

## Configure

```bash
cp .env.example .env
```

```dotenv
WIFI_SSID=your-ssid          # 2.4 GHz only
WIFI_PASS=your-password
WHATSAPP_WS_URL=wss://192.168.0.4:8080/ws/chat   # optional; mock default (with `mock-server`) or gateway
ADMIN_TOKEN=                                     # optional; see "Securing the dashboard"
```

Everything here is baked in at build time, so a change means rebuild plus
reflash. The project builds without a `.env` but will not connect until WiFi is
set.

The admin token can also be flashed in without a rebuild, as an `admin_token`
string in the `wa` namespace of the default NVS partition. That keeps it out of
the firmware image:

```bash
printf 'key,type,encoding,value\nwa,namespace,,\nadmin_token,data,string,kitchen-secret\n' > nvs.csv
python -m esp_idf_nvs_partition_gen generate nvs.csv nvs.bin 0x6000
espflash write-bin 0x9000 nvs.bin        # the `nvs` partition in partitions.csv
```

Which server to trust is the `mock-server` cargo feature, off by default. With
it the firmware talks to the mock URL with no CA configured, so esp-tls skips
verification entirely (the mock mints a fresh self-signed cert per start, so
pinning is pointless), `whatsapp-rust` skips the Noise cert check, and the QR
is auto-scanned:

```bash
cargo build --release --features mock-server
```

Without it the firmware talks to the real gateway and verifies both the TLS
chain and the Noise certificates.

### Securing the dashboard

Port 8081 is open to the LAN. Without a token it reads recent messages, sends
as the account and factory-resets the device. Set one:

```dotenv
ADMIN_TOKEN=something-long-and-random
```

Then `/send`, `/messages`, `/pair-code`, `/reset`, `/reboot` and `/sessions`
answer `401` without an `X-Admin-Token` header. The dashboard asks for the token
once and keeps it in session storage. Status routes (`/`, `/device`,
`/metrics`, `/health`) stay open so the page renders first, with pairing fields
redacted until the token is given. Leave it unset and the device works as
before, with a warning at boot.

A token does not fix two things. The API is plain HTTP, so everything crosses
the LAN in cleartext; keep the device on a trusted network. And `wa_store` is
unencrypted, so anyone reading the flash gets the pairing and the Signal state.
Physical access is full access.

## Build

```bash
cargo build --features mock-server              # S3 debug against the mock server
cargo build --release --features mock-server    # S3 release against the mock server
cargo build --release                           # S3 release against the real gateway
scripts/build.sh --board esp32c5 --release --features mock-server   # C5
```

A bare `cargo build` is the S3 build. Target, `build-std` and the `MCU` /
`ESP_IDF_VERSION` variables come from `.cargo/config.toml`.
`scripts/build.sh` repeats the same build for the named board (`BOARD=...` works
too, `CARGO_CMD=clippy` runs clippy instead). Each board gets its own ESP-IDF
tree and output dir, so switching boards costs disk only.

Reflashing the app keeps the device linked. Pairing, Signal state and sync keys
live in `wa_store`, separate from the app partition. Only a factory reset, or
erasing that partition, unlinks it.

For where the bytes go and which size levers paid off:
[docs/app-image-size.md](docs/app-image-size.md).

## Test without hardware

CI runs two layers on pull requests and pushes to `main`.

1. Build all five flavors with the pinned toolchain (S3, C5 and C3 board
   builds, plus a QEMU build per emulated chip) and fail if an image outgrows
   the factory partition. All five ELFs upload as artifacts.
2. Pair, persist and message on QEMU. Espressif's QEMU boots an emulated S3
   (8 MB PSRAM, OpenCores Ethernet) against the same mock server the
   `whatsapp-rust` E2E suite uses:
   1. Board `a` boots with an empty store, pairs over QR, reaches
      `Connected to WhatsApp!`.
   2. Board `a` reboots from the same flash image. It must log
      `WhatsApp NVS loaded: device=true`, connect without printing a QR, and
      report the same number. That is the persistence guarantee.
   3. Board `b` boots alongside with its own number. `POST /send` on `a`
      sends `🦀ping`. `b` reacts, replies quoting it and edits the reply.
      `a` sees the `🏓 Pong!` in `/messages`.

   Real instruction stream, real mbedTLS, real Noise handshake and real NVS
   writes in both directions. Only the radio and the crypto accelerators are
   missing (see below).

Locally, same flow through `scripts/qemu.sh`:

```bash
# once: Espressif's QEMU fork (upstream QEMU lacks these machines) and esptool.
R=https://dl.espressif.com/github_assets/espressif/qemu/releases/download/esp-develop-9.2.2-20260417
V=esp_develop_9.2.2_20260417-x86_64-linux-gnu

# S3 (Xtensa)
curl -sSfL -o qemu-xtensa.tar.xz "$R/qemu-xtensa-softmmu-$V.tar.xz"
mkdir -p ~/qemu-xtensa && tar -xJf qemu-xtensa.tar.xz -C ~/qemu-xtensa
export QEMU_XTENSA=~/qemu-xtensa/qemu/bin/qemu-system-xtensa

# C3 (RISC-V)
curl -sSfL -o qemu-riscv32.tar.xz "$R/qemu-riscv32-softmmu-$V.tar.xz"
mkdir -p ~/qemu-riscv32 && tar -xJf qemu-riscv32.tar.xz -C ~/qemu-riscv32
export QEMU_RISCV32=~/qemu-riscv32/qemu/bin/qemu-system-riscv32
# both need libsdl2, libslirp, glib and pixman at runtime
pip install esptool esp-idf-nvs-partition-gen   # the ESP-IDF python env under .embuild already has both

scripts/qemu.sh build      # release + `qemu` feature + sdkconfig.qemu, into target/qemu-esp32s3/
BOARD=esp32c3 scripts/qemu.sh all   # same, end to end, on the emulated C3
scripts/qemu.sh image a    # 16 MB flash image for board "a": bootloader + partition table + NVS + app
scripts/qemu.sh run a      # interactive serial console, Ctrl-A X quits
scripts/qemu.sh test       # headless: the three stages above (needs images a and b)
```

`run` reuses the image, so an interactively paired board stays paired across
runs. Delete `target/qemu-<board>/.../flash_image-a.bin` (or rerun
`scripts/qemu.sh image a`) for a fresh one.

The `qemu` feature changes one thing. The network comes up over emulated
Ethernet instead of WiFi, since QEMU has no radio, so no `.env` is needed and
the default server URL is the host as the guest sees it (`10.0.2.2`).
`WHATSAPP_WS_URL` still overrides it, `127.0.0.1`/`localhost` URLs from the
server are remapped there too, and the dashboard forwards to
`http://localhost:8081` (`b` gets 8082). `sdkconfig.qemu` enables the OpenCores
driver, switches the PSRAM probe to quad mode, and moves AES and SHA to
software because the emulated AES block never finishes a DMA transfer. Heap
routing and stacks stay as on hardware.

`QEMU_GDB=1 scripts/qemu.sh run` halts the CPUs with a gdb stub on port 1234.
Attach `xtensa-esp32s3-elf-gdb` for a backtrace of every FreeRTOS task, which
beats staring at a hung serial log.

The mock server must listen on host port 8080, where the `whatsapp-rust` E2E
setup puts it (see its `agent_docs/e2e_testing.md`). On forks CI skips
`qemu-e2e` because the mock image is private. The build job still runs.

QEMU cannot tell you about the radio, real flash/PSRAM timing, or power. Those
need the board.

## Flash and monitor

On Arch the user must be in `uucp` (`dialout` on Debian/Ubuntu).

The S3 and C5 show up as `/dev/ttyACM0` over built-in USB-Serial/JTAG (USB id
`303a:1001`). Most C3 devkits use a USB-serial bridge instead and appear as
`/dev/ttyUSB0`, which is why the C3 overlay leaves the console on UART0 (see
[docs/esp32c3.md](docs/esp32c3.md)). Below is the S3 path. Other boards add
`--chip` and use their target dir:

| Board | `--chip` | Target directory |
|-------|----------|------------------|
| ESP32-S3 | `esp32s3` (default) | `target/xtensa-esp32s3-espidf/<profile>/` |
| ESP32-C5 | `esp32c5` | `target/riscv32imac-esp-espidf/<profile>/` |
| ESP32-C3 | `esp32c3` | `target/riscv32imc-esp-espidf/<profile>/` |

```bash
espflash board-info --port /dev/ttyACM0   # chip type, flash size, MAC
```

Flash with our bootloader and partition table spelled out. Without
`--partition-table` espflash falls back to a 1.5 MB factory partition and the
~4.2 MB app does not fit:

```bash
espflash flash \
  --port /dev/ttyACM0 \
  --bootloader target/xtensa-esp32s3-espidf/debug/bootloader.bin \
  --partition-table partitions.csv \
  target/xtensa-esp32s3-espidf/debug/whatsapp-esp32
```

`espflash` converts the ELF on the fly. The `dev` profile already builds at
`opt-level = "z"`, so that path is the normal one (`release/` if you built
with `--release`). A good flash prints something like
`App/part. size 4,237,872/4,980,736 (85.09%)`. The second number is our 4864K
partition, which confirms it is in use. Add `--baud 921600` to go faster,
`--monitor` to land in the console afterwards.

### Watching the serial log

Interactive: append `--monitor` to the flash command, or run
`espflash monitor --port /dev/ttyACM0 --elf target/.../whatsapp-esp32`.
The `--elf` symbolizes panic backtraces. `CTRL+R` resets, `CTRL+C` exits.

Headless: `espflash monitor` wants to sync with the bootloader, so reset and
read the raw device instead (USB-CDC ignores baud):

```bash
espflash reset --port /dev/ttyACM0          # restart the app
timeout 20 cat /dev/ttyACM0                  # boot log: WiFi, IP, admin URL, heap
```

A healthy boot ends with `WiFi connected! IP: <ip>`, the admin server on port
8081 and `Bot built, starting run loop`. With no server reachable you get TLS
failures and exponential backoff instead. That is still a good sign. It means
the device is alive and its timers work.

## Pair and use

1. Flash, then watch the serial log or dashboard for the QR code.
2. On the phone: WhatsApp > Linked Devices > Link a Device, and scan it. No
   camera handy: the dashboard's "Link with phone number" form fetches an
   8-character code instead.
3. Dashboard at `http://esp32-whatsapp.local:8081/dashboard` (mDNS) or
   `http://<device-ip>:8081/dashboard`. QR, paired PN/LID, recent messages,
   free heap, session counts, a send box, and Clear Sessions / Factory Reset /
   Reboot.
4. Send `🦀ping` from a linked chat. The device reacts with 🏓, replies
   `🏓 Pong!` quoting you, then edits the reply with the send latency.
5. Reboot or power-cycle it. It comes back linked. Pairing, Signal sessions
   and sync keys live in `wa_store`. Only a factory reset, or the server
   unlinking the device, erases them.

## Admin endpoints

The firmware serves the dashboard and its QR renderer locally. The browser needs
only access to the device, not internet access. QR contents and the admin token
are never sent to an external QR service. The ESP32 still needs internet access
to connect to WhatsApp. See [QR asset provenance](src/assets/README.md) for the
vendored source, licenses, and browser test instructions.
The API on port 8081:

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/dashboard` | The HTML dashboard. |
| GET | `/qrcode.min.js` | Embedded QR renderer, no CDN dependency. |
| GET | `/` | JSON store stats (heap, sessions, identities, prekeys, paired). |
| GET | `/device` | Pairing status: QR, connection, PN/LID, linking-code state (redacted without token). |
| GET | `/messages` | Last 16 inbound messages. Needs the token. |
| POST | `/send` | `{"to":"<jid>","text":"..."}`. Sends a text, returns `message_id`. Needs the token. |
| POST | `/pair-code` | `{"phone_number":"+15551234567"}`. Requests a linking code, poll `/device`. Needs the token. |
| GET | `/metrics` | Live telemetry (see below). |
| GET | `/health` | Liveness check (`ok`). |
| GET | `/sessions` | List Signal session addresses. Needs the token. |
| DELETE | `/sessions` | Disconnect, erase Signal sessions from flash, reboot. Needs the token. |
| POST | `/reset` | Factory reset: log out, erase `wa_store`, reboot to re-pair. Needs the token. |
| POST | `/reboot` | Disconnect cleanly and reboot. Needs the token. |
| POST | `/test-panic` | Panic on purpose, to exercise crash capture (see below). |

Quick check from the LAN (mDNS `.local` often fails across routers or bands,
so prefer the IP from the boot log):

```bash
curl http://<device-ip>:8081/         # {"status":"running","heap_free":...}
curl http://<device-ip>:8081/device   # {"connected":...,"qr_code":...}
curl -H 'Content-Type: application/json' -d '{"to":"15551234567@s.whatsapp.net","text":"hi"}' http://<device-ip>:8081/send
curl -X POST http://<device-ip>:8081/reboot
```

`/reset`, `DELETE /sessions` and `/reboot` answer `202` at once and finish on
the executor. The live client logs out or disconnects first, flash is erased
while it is offline, and the reboot runs from a thread with an internal-RAM
stack (PSRAM is unreachable during restart). Concurrent requests merge into the
most destructive one.

## Diagnostics

`GET /metrics` reads straight from ESP-IDF, nothing estimated:

```bash
curl http://<device-ip>:8081/metrics
# {"reset_reason":"PowerOn","uptime_s":120,"heap_free":3493328,
#  "heap_internal_free":29775,"heap_min_free":3299632,
#  "internal_largest_block":7680,"internal_min_free":4755,
#  "psram_free":3463948,"rssi_dbm":-65}
# (full document also has `internal_8bit_*`, `psram_largest_block`,
# per-thread `stack_*_min`, `last_panic`, `coredump`)
```

`internal_*` is the scarce resource: internal DRAM in the tens of KB, separate
from the 8 MB PSRAM. `internal_min_free` is the all-time low. The AES/TLS and
prekey paths all fight over it, so that is what OOMs first. Watch it.

Crash cause is captured, not guessed. A panic hook logs the Rust panic with
location and message before aborting. `reset_reason` (also in the boot log as
`last reset: ...`) says why the previous run ended: `Panic`,
`TaskWatchdog`, `Brownout`, `PowerOn`. Hardware exceptions print a
`Backtrace: 0x...` of PCs, symbolized with the monitor
(`espflash monitor --elf target/.../whatsapp-esp32`) or directly:

```bash
xtensa-esp32s3-elf-addr2line -fCe target/xtensa-esp32s3-espidf/debug/whatsapp-esp32 0x42002fe2 0x...
```

## Troubleshooting

- `Stack canary watchpoint triggered` or stack overflow: grow the executor
  thread (`Esp32Executor::default_thread_config`, 256 KB; the quoted-reply plus
  edit path runs deep).
- TLS handshake fails against the mock (`mbedtls_ssl_handshake returned
  -0x2700`): the mock regenerates its self-signed cert every start, so no
  pinned CA can verify it. Build with `--features mock-server` and confirm
  `CONFIG_ESP_TLS_INSECURE=y` plus `CONFIG_ESP_TLS_SKIP_SERVER_CERT_VERIFY=y`
  are set, then rebuild and reflash.
- `AtomicU64` link errors from a dependency: Xtensa has no 64-bit atomics.
  Dependencies must use `portable_atomic`; this project pins it with `fallback`.
- ESP-IDF build fails on new host toolchains: v5.5.5 wants cmake < 4 and
  Python <= 3.12 (or 3.14 under the venv esp-idf-sys provisions).
- `Failed to open WhatsApp NVS ... rebooting to retry` in a loop: the
  `wa_store` partition is unreadable, usually a partition table from before it
  existed. The firmware never erases it on its own, since that would silently
  unlink the device. Erase deliberately with
  `espflash erase-parts --partition-table target/.../partition-table.bin wa_store`
  and pair again.

## License

MIT, same as [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust).
See [LICENSE](LICENSE).
