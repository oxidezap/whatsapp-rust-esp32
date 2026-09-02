use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use tungstenite::Message;
// Re-exported through whatsapp-rust so the `async_channel::Receiver` handed back
// by `create_transport` is provably the same type the client expects.
use whatsapp_rust::async_channel::{self, Sender};
use whatsapp_rust::async_trait;
use whatsapp_rust::bytes::Bytes;
use whatsapp_rust::wacore::net::{DisconnectReason, Transport, TransportEvent, TransportFactory};

/// TLS stream backed by ESP-IDF's mbedTLS.
/// Default bound on establishing a connection (TCP connect plus TLS handshake).
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct EspTlsStream {
    tls: *mut esp_idf_svc::sys::esp_tls_t,
}

unsafe impl Send for EspTlsStream {}

impl EspTlsStream {
    pub fn connect(host: &str, port: u16, skip_tls_verify: bool) -> Result<Self> {
        Self::connect_with_timeout(host, port, skip_tls_verify, CONNECT_TIMEOUT)
    }

    /// `timeout` bounds establishing the connection: the TCP connect and the TLS
    /// handshake. Without it a route that silently drops SYNs parks the calling
    /// thread indefinitely, before any socket option could apply.
    pub fn connect_with_timeout(
        host: &str,
        port: u16,
        skip_tls_verify: bool,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        let mut cfg = esp_idf_svc::sys::esp_tls_cfg_t {
            timeout_ms: timeout.as_millis().try_into().unwrap_or(i32::MAX),
            ..Default::default()
        };

        if skip_tls_verify {
            // Mock server: it mints a fresh ephemeral self-signed cert on every start,
            // so pinning a bundled CA is pointless (and was the cause of the -0x2700
            // "Failed to verify peer certificate"). We leave cfg WITHOUT any CA source
            // (cacert_buf / crt_bundle_attach / use_global_ca_store all unset), which
            // makes esp-tls fall through to MBEDTLS_SSL_VERIFY_NONE, but only when
            // CONFIG_ESP_TLS_SKIP_SERVER_CERT_VERIFY=y (see sdkconfig.defaults).
            // skip_common_name also avoids SNI/CN matching against the IP we dial.
            cfg.skip_common_name = true;
        } else {
            cfg.crt_bundle_attach = Some(esp_idf_svc::sys::esp_crt_bundle_attach);
        }

        let tls = unsafe { esp_idf_svc::sys::esp_tls_init() };
        if tls.is_null() {
            return Err(anyhow!("esp_tls_init failed"));
        }

        let host_cstr = std::ffi::CString::new(host)?;
        let ret = unsafe {
            esp_idf_svc::sys::esp_tls_conn_new_sync(
                host_cstr.as_ptr(),
                host.len() as i32,
                port as i32,
                &cfg,
                tls,
            )
        };

        if ret != 1 {
            unsafe { esp_idf_svc::sys::esp_tls_conn_destroy(tls) };
            return Err(anyhow!("TLS connection failed (ret={})", ret));
        }

        Ok(Self { tls })
    }

    pub fn set_read_timeout_ms(&self, ms: u32) -> Result<()> {
        self.set_socket_timeout_ms(esp_idf_svc::sys::SO_RCVTIMEO as i32, "SO_RCVTIMEO", ms)
    }

    /// The send-side counterpart. Without it a peer that accepts the connection
    /// and then stops reading parks the calling thread in `write` forever, which
    /// on the media-upload path is the executor.
    pub fn set_write_timeout_ms(&self, ms: u32) -> Result<()> {
        self.set_socket_timeout_ms(esp_idf_svc::sys::SO_SNDTIMEO as i32, "SO_SNDTIMEO", ms)
    }

    fn set_socket_timeout_ms(&self, option: i32, option_name: &str, ms: u32) -> Result<()> {
        let mut sockfd: i32 = 0;
        let err = unsafe { esp_idf_svc::sys::esp_tls_get_conn_sockfd(self.tls, &mut sockfd) };
        if err != 0 {
            return Err(anyhow!("esp_tls_get_conn_sockfd failed: {}", err));
        }

        let tv = esp_idf_svc::sys::timeval {
            tv_sec: (ms / 1000) as _,
            tv_usec: ((ms % 1000) * 1000) as _,
        };
        let ret = unsafe {
            esp_idf_svc::sys::lwip_setsockopt(
                sockfd,
                esp_idf_svc::sys::SOL_SOCKET as i32,
                option,
                &tv as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&tv) as u32,
            )
        };
        if ret != 0 {
            return Err(anyhow!("setsockopt {option_name} failed"));
        }
        Ok(())
    }
}

impl Read for EspTlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let ret = unsafe {
            esp_idf_svc::sys::esp_tls_conn_read(self.tls, buf.as_mut_ptr() as _, buf.len())
        };
        if ret < 0 {
            let esp_err = -ret;
            if esp_err == 11 || esp_err == 0x6900 {
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
            }
            Err(std::io::Error::other(format!(
                "esp_tls_conn_read error: {}",
                esp_err
            )))
        } else {
            Ok(ret as usize)
        }
    }
}

impl Write for EspTlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ret =
            unsafe { esp_idf_svc::sys::esp_tls_conn_write(self.tls, buf.as_ptr() as _, buf.len()) };
        if ret < 0 {
            Err(std::io::Error::other(format!(
                "esp_tls_conn_write error: {}",
                -ret
            )))
        } else {
            Ok(ret as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for EspTlsStream {
    fn drop(&mut self) {
        unsafe {
            esp_idf_svc::sys::esp_tls_conn_destroy(self.tls);
        }
    }
}

/// Transport implementation using ESP-IDF TLS + tungstenite WebSocket.
pub struct Esp32Transport {
    data_tx: std::sync::mpsc::Sender<Bytes>,
    shutdown: Arc<AtomicBool>,
}

#[async_trait]
impl Transport for Esp32Transport {
    async fn send(&self, data: Bytes) -> Result<(), anyhow::Error> {
        // Move the Bytes through the channel (refcount bump, no copy); tungstenite
        // 0.29's Message::Binary takes Bytes directly, so no realloc on either end.
        self.data_tx
            .send(data)
            .map_err(|_| anyhow!("Transport channel closed"))
    }

    async fn disconnect(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

pub struct Esp32TransportFactory {
    ws_url: String,
    skip_tls_verify: bool,
    thread: ThreadSpawnConfiguration,
}

impl Esp32TransportFactory {
    /// Connect to `ws_url`; `skip_tls_verify` accepts any server certificate
    /// (for a local mock server only; the real gateway must be verified).
    pub fn new(ws_url: impl Into<String>, skip_tls_verify: bool) -> Self {
        Self {
            ws_url: ws_url.into(),
            skip_tls_verify,
            thread: Self::default_thread_config(),
        }
    }

    /// The thread each connection's socket is driven on: 16 KB of stack on
    /// core 0, at the same priority as the executor. Without PSRAM the stack is
    /// internal DRAM and 12 KB; the frames here are mbedTLS record handling and
    /// `tungstenite` framing, neither of which recurses.
    pub fn default_thread_config() -> ThreadSpawnConfiguration {
        ThreadSpawnConfiguration {
            name: Some(c"ws-transport"),
            stack_size: crate::runtime::by_ram(16 * 1024, 12 * 1024),
            priority: 5,
            inherit: false,
            pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
            stack_alloc_caps: crate::runtime::stack_caps(),
        }
    }

    /// Drive the socket on a thread of the caller's choosing instead of
    /// [`Esp32TransportFactory::default_thread_config`]. The dashboard's
    /// `/metrics` reports the stack high-water mark of the thread named
    /// `ws-transport`; another name just drops out of that report.
    pub fn with_thread_config(mut self, thread: ThreadSpawnConfiguration) -> Self {
        self.thread = thread;
        self
    }
}

/// The production WhatsApp gateway (`wacore::net::WHATSAPP_WEB_WS_URL`), with
/// the server certificate verified against ESP-IDF's root bundle.
impl Default for Esp32TransportFactory {
    fn default() -> Self {
        Self::new(whatsapp_rust::wacore::net::WHATSAPP_WEB_WS_URL, false)
    }
}

#[async_trait]
impl TransportFactory for Esp32TransportFactory {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        let (event_tx, event_rx) = async_channel::unbounded();
        let (data_tx, data_rx) = std::sync::mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_clone = shutdown.clone();
        let ws_url = self.ws_url.clone();
        let skip_tls = self.skip_tls_verify;
        crate::runtime::spawn_thread(&self.thread, move || {
            ws_thread(event_tx, data_rx, shutdown_clone, &ws_url, skip_tls);
        })?;

        let transport = Arc::new(Esp32Transport { data_tx, shutdown });
        Ok((transport, event_rx))
    }
}

fn ws_thread(
    event_tx: Sender<TransportEvent>,
    data_rx: std::sync::mpsc::Receiver<Bytes>,
    shutdown: Arc<AtomicBool>,
    ws_url: &str,
    skip_tls_verify: bool,
) {
    let (host, port, _path, _tls) = match crate::http_client::parse_url(ws_url) {
        Ok(v) => v,
        Err(e) => {
            log::error!("Invalid WS URL: {}", e);
            let _ = event_tx.send_blocking(TransportEvent::Disconnected(
                DisconnectReason::ReadError(format!("invalid WS URL: {e}")),
            ));
            return;
        }
    };

    log::info!("WS thread: connecting to {}:{}...", host, port);

    let stream = match EspTlsStream::connect(&host, port, skip_tls_verify) {
        Ok(s) => s,
        Err(e) => {
            log::error!("TLS connect failed: {}", e);
            let _ = event_tx.send_blocking(TransportEvent::Disconnected(
                DisconnectReason::ReadError(format!("TLS connect failed: {e}")),
            ));
            return;
        }
    };

    log::info!("WS thread: TLS connected, starting WebSocket handshake...");

    let request = tungstenite::http::Request::builder()
        .uri(ws_url)
        .header("Origin", format!("https://{}", host))
        .header("Host", &host)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .unwrap();

    // tungstenite defaults to a 128 KB read buffer AND a 128 KB write buffer.
    // On a PSRAM board those come out of the 8 MB external heap and nobody pays
    // attention; on the ESP32-C3 a single 128 KB request is most of the free heap
    // and aborts the firmware ("memory allocation of 131072 bytes failed") right
    // after the handshake succeeds. Neither buffer caps message size -- the read
    // one is the chunk size reads are issued in, and the write one is the
    // threshold past which tungstenite stops coalescing -- so a smaller pair
    // costs syscalls, not capability, and WhatsApp's frames are nowhere near
    // either figure. The default is kept where there is PSRAM to spend, so the
    // boards this was tuned on keep the behaviour they were tested with.
    let ws_config = tungstenite::protocol::WebSocketConfig::default()
        .read_buffer_size(crate::runtime::by_ram(128 * 1024, 8 * 1024))
        .write_buffer_size(crate::runtime::by_ram(128 * 1024, 8 * 1024));

    let (mut ws, _response) = match tungstenite::client::client_with_config(request, stream, Some(ws_config))
    {
        Ok(ws) => ws,
        Err(e) => {
            log::error!("WebSocket handshake failed: {}", e);
            let _ = event_tx.send_blocking(TransportEvent::Disconnected(
                DisconnectReason::ReadError(format!("WebSocket handshake failed: {e}")),
            ));
            return;
        }
    };

    if let Err(e) = ws.get_mut().set_read_timeout_ms(100) {
        log::warn!("Could not set read timeout: {}", e);
    }

    log::info!("WS thread: WebSocket connected!");
    let _ = event_tx.send_blocking(TransportEvent::Connected);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            break;
        }

        while let Ok(data) = data_rx.try_recv() {
            log::debug!("--> WS send {} bytes", data.len());
            if let Err(e) = ws.send(Message::Binary(data)) {
                log::error!("WS send error: {}", e);
                let _ = event_tx.send_blocking(TransportEvent::Disconnected(
                    DisconnectReason::ReadError(format!("WS send error: {e}")),
                ));
                return;
            }
        }

        match ws.read() {
            Ok(msg) => match msg {
                Message::Binary(data) => {
                    log::debug!("<-- WS recv {} bytes", data.len());
                    let _ = event_tx.send_blocking(TransportEvent::DataReceived(data));
                }
                Message::Ping(data) => {
                    let _ = ws.send(Message::Pong(data));
                }
                Message::Close(frame) => {
                    log::info!("WS thread: server sent close");
                    let reason = frame
                        .map(|f| DisconnectReason::ServerClose {
                            code: Some(u16::from(f.code)),
                            reason: f.reason.to_string(),
                        })
                        .unwrap_or(DisconnectReason::ServerClose {
                            code: None,
                            reason: String::new(),
                        });
                    let _ = event_tx.send_blocking(TransportEvent::Disconnected(reason));
                    return;
                }
                _ => {}
            },
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Read timeout, normal
            }
            Err(e) => {
                log::error!("WS read error: {}", e);
                let _ = event_tx.send_blocking(TransportEvent::Disconnected(
                    DisconnectReason::ReadError(format!("WS read error: {e}")),
                ));
                return;
            }
        }
    }

    let _ = event_tx.send_blocking(TransportEvent::Disconnected(DisconnectReason::Unknown));
    log::info!("WS thread: exiting");
}
