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

A WhatsApp client running on **ESP32-S3** and **ESP32-C5** microcontrollers,
built on top of [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust).

It pairs over QR code or a phone-number linking code, keeps the pairing and its
Signal state in flash so a reboot comes back as the same linked device, connects
to WhatsApp (or a local mock server) over an encrypted WebSocket, runs a small
demo bot, and serves a status dashboard over HTTP. See
[Pair and use](#pair-and-use) for the bot behavior and dashboard.

> **Heads up:** this is a demonstration and research project, not a product, and it
> is not affiliated with, endorsed by, or connected to WhatsApp or Meta in any way.
> Running an unofficial client against the real WhatsApp service can break their Terms
> of Service and may get the number banned, so test with a spare number and the local
> mock server. The `mock-server` cargo feature is deliberately insecure for that
> local setup (any TLS certificate is accepted, the Noise server certificate is not
> verified, and the firmware scans its own pairing QR); it is off by default, and a
> build without it is what you point at the real gateway.

## Hardware

| Board | Chip | Flash / PSRAM | ESP-IDF | Build |
|-------|------|---------------|---------|-------|
| ESP32-S3 N16R8 devkit | Xtensa LX7, dual core | 16 MB / 8 MB octal | v5.5.5 | `cargo build --release --features mock-server` |
| [Waveshare ESP32-C5-Touch-LCD-2.8](https://github.com/waveshareteam/ESP32-C5-Touch-LCD-2.8) N16R8 | RISC-V, single core | 16 MB / 8 MB quad | v5.5.5 | `scripts/build.sh --board esp32c5 --release --features mock-server` |

The PSRAM is required on both: the main async task runs on a 256 KB stack
allocated from PSRAM, which is far larger than internal SRAM can provide. The
source is the same for both boards; both share ESP-IDF v5.5.5. What differs is
the target triple and one chip-specific `sdkconfig.defaults.<chip>` overlay (PSRAM
mode, console, cache layout), which `esp-idf-sys` picks up from the `MCU` it is
building for. Adding a board is adding that one file.

Which other Espressif parts could host this firmware, what each would cost, and
which emulator can stand in for it in CI: [docs/board-support-map.md](docs/board-support-map.md).
There is a hard floor: the chip needs PSRAM (the Rust heap and the 256 KB
executor stack live there) and at least 8 MB of flash (the app image alone is
4.5 MB), which rules out the whole C2/C3/C6/H2 line.

## How it works

`whatsapp-rust` is platform-agnostic: the protocol engine is written against
four traits, and a platform supplies them. This repository is one crate with two
targets. The library, `whatsapp_esp32`, is the ESP32 platform: exactly those four
implementations, plus the pieces a firmware around them tends to need. The
binary, `whatsapp-esp32`, is the demo firmware built on it.

| Module | Provides | `whatsapp-rust` contract |
|--------|----------|--------------------------|
| `storage` | `NvsStore` | `Backend`. The linked device, the Signal state and the app-state sync keys live in an NVS partition (`wa_store` by default) and survive reboots; the rest is a RAM cache. |
| `transport` | `Esp32TransportFactory` | `TransportFactory`. ESP-IDF mbedTLS + `tungstenite` WebSocket, driven on its own thread. |
| `http_client` | `EspHttpClient` | `HttpClient`. Streaming HTTP/1.1 over ESP-IDF TLS/TCP with bounded RAM (media, version fetch). |
| `runtime` | `Esp32Runtime`, `Esp32Executor`, `BlockingWorker` | `Runtime`. `edge-executor` event loop that parks when idle; `spawn_blocking` runs on a dedicated `wa-blocking` thread so key generation never stalls the loop. |
| `psram_alloc` | `PsramAllocator` | Optional global allocator that keeps the Rust heap in PSRAM. |
| `supervisor` | `DeviceStatus`, `ActiveClient`, `MaintenanceCoordinator` | Firmware bookkeeping: which client is live, what the dashboard shows, the one path that erases or reboots. |
| `metrics`, `crash` | system telemetry, panic and core-dump capture | What the dashboard's `/metrics` reports. |
| `admin` (feature `admin`, default on) | `start_admin_server` | The HTTP dashboard and API. |
| `src/main.rs` | the demo firmware | WiFi + SNTP + mDNS bringup, the executor thread, a supervisor that rebuilds the bot when it exits, the ping/pong bot. |

## Using it as a library

There is no wrapper around `Bot`, no ESP32 event type and no extension trait:
a firmware on this crate writes the same `Bot::builder()` code as a desktop
program, with the four platform values plugged in, and reads `whatsapp-rust`'s
own documentation for everything else.

```toml
[dependencies]
# Both from git, at the same whatsapp-rust revision this crate's Cargo.toml
# names: two different whatsapp_rust packages in one build would give the
# Bot builder trait objects the platform types do not implement.
whatsapp-rust = { git = "https://github.com/oxidezap/whatsapp-rust", rev = "ec72862c315cfea50c27404a0777a6f9bfae4d84", default-features = false }
whatsapp-esp32 = { git = "https://github.com/oxidezap/whatsapp-rust-esp32", default-features = false }
esp-idf-svc = { version = "0.52", features = ["binstart", "critical-section"] }
anyhow = "1"
log = "0.4"
```

`examples/minimal.rs`, which CI compiles for the target (this is a copy):

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

    // The executor needs a large stack (the send path has deep frames), which
    // only PSRAM can afford; `default_thread_config` is 256 KB there.
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

`Esp32TransportFactory::default()` and `EspHttpClient::default()` talk to the
production gateway with the server certificate verified against ESP-IDF's root
bundle; `::new(url, skip_tls_verify)` is for a local mock server. The threads the
library starts (`ws-transport`, `wa-blocking`) can be given other stacks,
priorities or cores through `Esp32TransportFactory::with_thread_config` and
`BlockingWorker::start_with`, both taking esp-idf-hal's own
`ThreadSpawnConfiguration`.

What the firmware around it has to provide, all of which `src/main.rs`,
`sdkconfig.defaults` and `partitions.csv` show working:

- **PSRAM**, enabled for `malloc` and for task stacks
  (`CONFIG_SPIRAM_USE_MALLOC`, `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM`), and a
  large stack for the executor thread (`Esp32Executor::default_thread_config`
  takes 256 KB there).
- **A `wa_store` NVS partition** (1 MB here), or any name passed to `NvsStore::open`.
- **mbedTLS allocating from external memory** (`CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC`),
  and the **task watchdog not checking the idle task** on the executor's core,
  since the blocking worker runs below idle priority there.
- **Time**: the Noise handshake needs a roughly correct clock, so start SNTP
  before the first connect.

Since `whatsapp-rust` 0.7.0 a single upstream dependency is enough: the crate re-exports
`wacore`, `waproto`, `buffa` and the shared support crates, so they can never
resolve to a different version than the one it was built against. It is pinned
to a git revision in `Cargo.toml` (the builder options this firmware relies on
are newer than the 0.7.0 release) with `default-features = false`, which drops
the desktop-only features (tokio, SQLite, ureq, SIMD). A consumer of the library
must name the same revision.

Everything is then reached through it — `whatsapp_rust::wacore::net`,
`whatsapp_rust::prelude::{wa, MessageField, ...}`, `whatsapp_rust::async_trait`,
and so on. `anyhow` and `futures` remain direct dependencies because this crate
needs feature flags (`anyhow/std`, `futures/executor`) that `whatsapp-rust` does
not enable.

Requires a Rust toolchain of at least **1.94** (the 0.7.0 workspace MSRV; its
crates are edition 2024). The Xtensa `esp` channel is well past that.

## Prerequisites

- The Espressif Rust toolchain: `cargo install espup && espup install`
  (`rust-toolchain.toml` pins `channel = "esp"`; the same toolchain carries the
  RISC-V target the C5 uses).
- `cargo install ldproxy` (the linker wrapper referenced by `.cargo/config.toml`).
- `cargo install espflash` for flashing and monitoring.
- ESP-IDF is downloaded and built automatically by `esp-idf-sys` on the first
  build (into `.embuild/`, a few GB; subsequent builds reuse it): **v5.5.5**
  unified for both the ESP32-S3 and the ESP32-C5.
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
WHATSAPP_WS_URL=wss://192.168.0.4:8080/ws/chat   # optional; defaults to the mock (with `mock-server`) or the gateway
WHATSAPP_PUSH_NAME=esp32-test                    # optional; the name the device pairs under
ADMIN_TOKEN=                                     # optional; see "Securing the dashboard"
```

These are read at build time and baked into the firmware, so changing them needs
a rebuild + reflash.

The push name alone can also be set at flash time, without a rebuild, as a string
in the default NVS partition (namespace `wa`, key `push_name`). Against the mock
server the push name selects the account, so this is how several boards flashed
with one image end up as different numbers; it is also what the two-board QEMU
test uses. `esp-idf-nvs-partition-gen` (pip) turns a CSV into the partition image:

```bash
printf 'key,type,encoding,value\nwa,namespace,,\npush_name,data,string,kitchen-esp32\n' > nvs.csv
python -m esp_idf_nvs_partition_gen generate nvs.csv nvs.bin 0x6000
espflash write-bin 0x9000 nvs.bin        # the `nvs` partition in partitions.csv
```

Which server the firmware trusts is the `mock-server` cargo feature, off by
default. With it the firmware defaults to the mock server URL and connects with
**no** CA configured, so esp-tls applies `MBEDTLS_SSL_VERIFY_NONE` and accepts
whatever certificate the server presents (the mock server, `barback`, mints a
fresh ephemeral self-signed cert on every start, so there is nothing stable to pin
against; this relies on `CONFIG_ESP_TLS_INSECURE=y` +
`CONFIG_ESP_TLS_SKIP_SERVER_CERT_VERIFY=y` in `sdkconfig.defaults`),
`whatsapp-rust` skips Noise server-certificate verification, and the pairing QR is
auto-scanned:

```bash
cargo build --release --features mock-server
```

Without it (a bare `cargo build --release`) the firmware defaults to the real
WhatsApp gateway and verifies both the ESP-IDF root certificate bundle and the
Noise certificate chain.

### Securing the dashboard

The dashboard and its API listen on port 8081 for anyone who can reach the
device. That was already enough to factory-reset it, and this firmware also lets
that port read recent messages and send as the linked account, so there is a
shared secret you can require:

```dotenv
ADMIN_TOKEN=something-long-and-random
```

With one set, `/send`, `/messages`, `/pair-code`, `/reset`, `/reboot`, and
`/sessions` (both GET and DELETE) answer `401` unless the request carries
`X-Admin-Token`. The dashboard has a field for it and keeps it in that browser's
session storage. The status routes (`/`, `/device`, `/metrics`, `/health`) stay
open so the page can render before you type it in; sensitive pairing fields
(`qr_code`, `pair_code`, `pn`, `lid`) on `/device` are redacted until the token is provided.

Leave it unset and the device behaves as it always has, with a warning in the
boot log naming what is exposed. Like the push name, it can also be provisioned
at flash time as an `admin_token` string in the `wa` NVS namespace, which keeps
it out of the firmware image.

Two things a token does not fix: the API is plain HTTP, so the token and
everything else crosses the LAN in cleartext (restrict device access to trusted,
isolated local networks), and the `wa_store` partition is unencrypted, so anyone
who can read the flash can read the pairing and the Signal state. Treat physical
access to the board as full access to the account.

## Build

```bash
cargo build --features mock-server              # ESP32-S3, debug (opt-level "z", fat LTO, ~14 MB ELF with debug info)
cargo build --release --features mock-server    # ESP32-S3, release, against the local mock server
cargo build --release                           # ESP32-S3, release, against the real gateway (see "Configure")
scripts/build.sh --board esp32c5 --release --features mock-server   # ESP32-C5 (riscv32imac-esp-espidf, ESP-IDF v5.5.5)
```

The S3 target (`xtensa-esp32s3-espidf`), `build-std`, and the `MCU` /
`ESP_IDF_VERSION` environment variables all come from `.cargo/config.toml`, so a
bare `cargo build` is the S3 build. `scripts/build.sh` is the same `cargo build`
with the target and those two variables switched for the board named (`BOARD=...`
in the environment works too, and `CARGO_CMD=clippy` runs clippy instead). The
two boards' ESP-IDF trees and build outputs never share a directory, so
switching between them costs nothing but disk.

What persists across a reflash of the app: the `wa_store` partition (the pairing,
Signal state and sync keys) is separate from the app, so flashing a new build
keeps the device linked. A factory reset from the dashboard, or erasing that
partition, is what unlinks it.

## Test without hardware

Two layers stand in for a board, and both run in CI (`.github/workflows/ci.yml`)
on pull requests and on pushes to `main`:

1. **Build for the real targets.** The `build` job compiles the firmware with
   the pinned `esp` toolchain in three flavors, the ESP32-S3 board build, the
   ESP32-C5 board build and the QEMU build, and fails when an app image no
   longer fits the factory partition (`scripts/check-app-size.sh`). All three
   ELFs are uploaded as artifacts.
2. **Pair, persist and message on QEMU.** The `qemu-e2e` job runs the QEMU
   flavor on [Espressif's QEMU](https://github.com/espressif/esp-toolchain-docs/tree/main/qemu/esp32s3)
   (an ESP32-S3 with 8 MB PSRAM and an OpenCores Ethernet MAC) against the same
   mock server `whatsapp-rust`'s E2E suite uses, in three stages:
   1. Board `a` boots with an empty `wa_store`, pairs over the QR flow and
      reaches `Connected to WhatsApp!`; its dashboard must report the session.
   2. Board `a` is stopped and booted again **from the same flash image**. It
      must log `WhatsApp NVS loaded: device=true`, connect without ever
      printing a QR code, and report the same number as before. That is the
      persistence guarantee: what the firmware wrote to the emulated flash on
      the first boot is what it reads back on the second.
   3. Board `b`, provisioned with a different push name and so a different
      number, boots alongside. `POST /send` on `a` sends it `🦀ping`; `b`'s
      bot must receive it, react, reply quoting it and edit the reply; `a`
      must see the `🏓 Pong!` land in its own `/messages`.

   Nothing in that path is a stub: the instruction stream, the ESP-IDF build,
   mbedTLS, the Noise handshake, the Signal key generation, the NVS writes and
   the message encryption in both directions all run as they would on the chip.

The same flow runs locally with `scripts/qemu.sh`:

```bash
# once: Espressif's QEMU (the esp32s3 machine is not in upstream QEMU) and esptool
curl -sSfL -o qemu.tar.xz https://dl.espressif.com/github_assets/espressif/qemu/releases/download/esp-develop-9.2.2-20260417/qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz
mkdir -p ~/qemu && tar -xJf qemu.tar.xz -C ~/qemu     # needs libsdl2, libslirp, glib, pixman at runtime
export QEMU_XTENSA=~/qemu/qemu/bin/qemu-system-xtensa
pip install esptool esp-idf-nvs-partition-gen   # the ESP-IDF python env under .embuild already has both

scripts/qemu.sh build      # release build with --features qemu and sdkconfig.qemu, into target/qemu/
scripts/qemu.sh image a    # 16 MB flash image for board "a": bootloader + partition table + its NVS + app
scripts/qemu.sh run a      # interactive: serial console on the terminal, Ctrl-A X quits
scripts/qemu.sh test       # headless: the three stages above (needs images a and b)
```

`run` reuses the image, so a board you paired interactively stays paired across
runs, exactly as a real one would; delete `target/qemu/.../flash_image-a.bin` (or
`scripts/qemu.sh image a`) for a fresh one.

What the `qemu` feature changes, and nothing else: the network comes up over the
emulated Ethernet MAC instead of WiFi (QEMU has no radio), so no `.env` is needed,
and the default server URL becomes `wss://10.0.2.2:8080/ws/chat`, the host as seen
from QEMU's user-mode network. `WHATSAPP_WS_URL` still overrides it, and a media or app-state URL the server
hands out on `127.0.0.1`/`localhost` is dialed as `10.0.2.2` too, since inside the
emulator loopback is the guest. The dashboard is forwarded to
`http://localhost:8081` (board `b`: 8082), on loopback only, since it is unauthenticated. The overlay `sdkconfig.qemu` enables the
OpenCores driver, switches the PSRAM probe to quad mode (what QEMU's generic SPI
PSRAM answers), and moves mbedTLS's AES and SHA to software, because the emulated
AES block never completes a DMA transfer and every TLS connect would spin forever
in `aes_hal_wait_done()`. The heap routing, the 256 KB PSRAM stack and every
other setting stay as on the board, so what the emulator exercises is the same
firmware minus the radio and the crypto accelerators.

`QEMU_GDB=1 scripts/qemu.sh run` starts QEMU with a gdb stub on port 1234 and the
CPUs halted; `xtensa-esp32s3-elf-gdb` (from Espressif's binutils-gdb releases)
with `target remote :1234` then shows every FreeRTOS task's backtrace, which is a
far better view of a hang than the serial console.

For `test` and `run` the mock server has to be listening on the host's port 8080,
which is where the `whatsapp-rust` E2E setup puts it (see its
`agent_docs/e2e_testing.md`). On a fork, CI skips `qemu-e2e` because the mock
server image is private; the build job still runs.

What this cannot tell you: anything about the radio (WiFi association, RF
calibration, PMF), timing that depends on real flash and PSRAM latency, and power.
Those still need the board.

## Flash and monitor

Install `cargo install espflash`. On Arch your user must be in the `uucp` group to
access the serial port (`dialout` on Debian/Ubuntu).

Both boards expose a built-in USB-Serial/JTAG (USB id `303a:1001`), so they show up
directly as `/dev/ttyACM0`, with no external UART adapter needed. The commands below
are for the S3; for the C5 add `--chip esp32c5` and use the
`target/riscv32imac-esp-espidf/<profile>/` paths. Confirm the link first:

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
2. On your phone: WhatsApp > Linked Devices > Link a Device, and scan it. No
   camera at hand? The dashboard's "Link with phone number" form asks the server
   for an 8-character linking code instead (Link a Device > Link with phone
   number instead).
3. Open the dashboard at `http://esp32-whatsapp.local:8081/dashboard`
   (mDNS) or `http://<device-ip>:8081/dashboard`. It renders the QR, shows the
   paired PN/LID, the last inbound messages, free heap, and
   session/identity/prekey counts, lets you send a text, and exposes Clear
   Sessions / Factory Reset / Reboot actions.
4. From a linked chat, send `🦀ping`. The device reacts with 🏓, replies
   `🏓 Pong!` quoting your message, then edits the reply with the measured send
   latency.
5. Reboot or power-cycle it: it comes back linked. The pairing, the Signal
   sessions and the app-state sync keys are in the `wa_store` flash partition;
   only a factory reset (dashboard, or the server unlinking the device) erases
   them.

## Admin endpoints

The dashboard is backed by a small HTTP API on port 8081 (the dashboard page
itself pulls `qrcode.min.js` from a CDN, so the browser needs internet access):

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/dashboard` | The HTML dashboard. |
| GET | `/` | JSON store stats (heap, sessions, identities, prekeys, paired). |
| GET | `/device` | Pairing status: QR code, connection, PN/LID, linking-code state (pairing credentials redacted without token). |
| GET | `/messages` | The last 16 inbound messages (id, chat, sender, truncated text, timestamp). Needs the token. |
| POST | `/send` | `{"to":"<jid>","text":"..."}`: send a text and wait for the outcome (`message_id` on success). Needs the token. |
| POST | `/pair-code` | `{"phone_number":"+15551234567"}`: request a linking code; poll `/device` for it. Needs the token. |
| GET | `/metrics` | Live system telemetry (see Diagnostics below). |
| GET | `/health` | Liveness check (`ok`). |
| GET | `/sessions` | List Signal session addresses. Needs the token. |
| DELETE | `/sessions` | Disconnect, erase all Signal sessions from flash, reboot. Needs the token. |
| POST | `/reset` | Factory reset: log out, erase `wa_store`, reboot to re-pair. Needs the token. |
| POST | `/reboot` | Disconnect cleanly and reboot. Needs the token. |
| POST | `/test-panic` | Deliberately panic, to exercise the persistent crash capture (see Diagnostics). |

Handy for a quick liveness check from the same network, using the IP from the boot
log (mDNS `.local` resolution is unreliable across some routers / 2.4-vs-5 GHz SSIDs):

```bash
curl http://<device-ip>:8081/         # {"status":"running","heap_free":...}
curl http://<device-ip>:8081/device   # {"connected":...,"qr_code":...}
curl -H 'Content-Type: application/json' -d '{"to":"15551234567@s.whatsapp.net","text":"hi"}' http://<device-ip>:8081/send
curl -X POST http://<device-ip>:8081/reboot
```

The three maintenance actions (`/reset`, `DELETE /sessions`, `/reboot`) answer
`202` at once and finish on the client's executor: the live client is logged out
or disconnected first, the flash is erased with it offline, and the reboot runs
from a thread whose stack is in internal RAM (the chip disables PSRAM access
while restarting). Concurrent requests are merged into the most destructive one.

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

- **`Stack canary watchpoint triggered` / stack overflow:** give the executor
  thread more stack (`Esp32Executor::default_thread_config` is 256 KB; the full
  send path with a quoted reply and edit is stack-heavy).
- **TLS handshake fails against the mock server (`mbedtls_ssl_handshake returned
  -0x2700` / "Failed to verify peer certificate"):** the mock server regenerates its
  self-signed cert on every start, so verification cannot succeed against any pinned CA.
  Ensure the build has `--features mock-server` **and** that
  `CONFIG_ESP_TLS_INSECURE=y` + `CONFIG_ESP_TLS_SKIP_SERVER_CERT_VERIFY=y` are in
  `sdkconfig.defaults`, then rebuild + reflash. With those set the firmware skips cert
  verification entirely and the regenerated ephemeral cert no longer matters.
- **`AtomicU64` / 64-bit atomic link errors from a dependency:** Xtensa has no
  native 64-bit atomics; dependencies must use `portable_atomic`. The project
  relies on `portable-atomic` with the `fallback` feature for this.
- **ESP-IDF build fails on very new host toolchains:** ESP-IDF v5.5.5 is happiest
  with cmake < 4 and Python <= 3.12 (or Python 3.14 with the venv esp-idf-sys
  provisions). If your host ships newer ones, the `esp-idf-sys` bootstrap may complain.
- **`Failed to open WhatsApp NVS ... rebooting to retry` in a loop:** the
  `wa_store` partition is unreadable (a partition table from before it existed,
  or a corrupted page). The firmware never erases it on its own, because that
  would silently unlink the device; erase it deliberately with
  `espflash erase-parts --partition-table target/.../partition-table.bin wa_store`
  and pair again.
- **Two boards on the mock server land on the same number:** they pair under the
  same push name. Provision distinct ones (see Configure).

## License

MIT, the same license as [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust).
See [LICENSE](LICENSE).
