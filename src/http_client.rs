use std::io::{Read, Write};

use anyhow::Result;
use whatsapp_rust::async_trait;
use whatsapp_rust::wacore::net::{HttpClient, HttpRequest, HttpResponse, StreamingHttpResponse};

use crate::transport::EspTlsStream;

/// Parse a URL into (host, port, path, use_tls).
/// Supports http://, https://, ws://, wss:// schemes.
pub fn parse_url(url: &str) -> Result<(String, u16, String, bool)> {
    let (without_scheme, use_tls) = url
        .strip_prefix("https://")
        .map(|s| (s, true))
        .or_else(|| url.strip_prefix("wss://").map(|s| (s, true)))
        .or_else(|| url.strip_prefix("http://").map(|s| (s, false)))
        .or_else(|| url.strip_prefix("ws://").map(|s| (s, false)))
        .ok_or_else(|| anyhow::anyhow!("Invalid URL scheme: {}", url))?;

    let (host_port, path) = match without_scheme.find('/') {
        Some(i) => (&without_scheme[..i], &without_scheme[i..]),
        None => (without_scheme, "/"),
    };

    let (host, port) = match host_port.find(':') {
        Some(i) => (&host_port[..i], host_port[i + 1..].parse::<u16>()?),
        None => (host_port, if use_tls { 443 } else { 80 }),
    };

    Ok((host.to_string(), port, path.to_string(), use_tls))
}

/// Under QEMU, a URL that names the host's loopback has to be dialed as the
/// emulator's gateway instead. Only the dial address changes; the request's
/// Host header still carries the URL's own authority.
///
/// The mock server hands out media and app-state URLs on `https://127.0.0.1:8080`,
/// which is right for a client running on the same machine and wrong for one
/// running inside QEMU's user-mode network, where 127.0.0.1 is the guest itself
/// and the host is 10.0.2.2. Without this the app-state snapshot download fails
/// on every attempt, the critical sync never completes, and `Event::Connected`
/// never fires. Only the loopback names are touched; a board build has no
/// such mapping at all.
#[cfg(feature = "qemu")]
fn qemu_host(host: String) -> String {
    if host == "127.0.0.1" || host == "localhost" {
        "10.0.2.2".to_string()
    } else {
        host
    }
}

#[cfg(not(feature = "qemu"))]
fn qemu_host(host: String) -> String {
    host
}

pub struct EspHttpClient {
    skip_tls_verify: bool,
}

impl EspHttpClient {
    pub fn new(skip_tls_verify: bool) -> Self {
        Self { skip_tls_verify }
    }
}

#[async_trait]
impl HttpClient for EspHttpClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let (host, port, path, use_tls) = parse_url(&request.url)?;
        // The Host header keeps the URL's own authority; only the address dialed
        // is remapped under QEMU.
        let raw_request = build_raw_request(
            &request.method,
            &path,
            &host,
            &request.headers,
            request.body.as_deref(),
        );

        let dial = qemu_host(host);
        let mut stream = connect_stream(&dial, port, use_tls, self.skip_tls_verify)?;
        do_request(&mut stream, &raw_request, request.body.as_deref())
    }

    fn execute_streaming(&self, request: HttpRequest) -> Result<StreamingHttpResponse> {
        let (host, port, path, use_tls) = parse_url(&request.url)?;
        let raw_request = build_raw_request(&request.method, &path, &host, &request.headers, None);

        let dial = qemu_host(host);
        let stream = connect_stream(&dial, port, use_tls, self.skip_tls_verify)?;
        do_streaming_request(stream, &raw_request)
    }
}

/// A connected client stream: TLS via ESP-IDF mbedTLS, or plain TCP. A concrete
/// enum (not a boxed `dyn`) so it satisfies both `do_request`'s `Read + Write` and
/// `do_streaming_request`'s `Read + Write + Send + 'static` bounds, with static
/// dispatch and no extra allocation. `EspTlsStream` is `Send` (see transport.rs)
/// and `TcpStream` is `Send`, so the enum is `Send + 'static`.
enum HttpStream {
    Tls(EspTlsStream),
    Tcp(std::net::TcpStream),
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            HttpStream::Tls(s) => s.read(buf),
            HttpStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for HttpStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            HttpStream::Tls(s) => s.write(buf),
            HttpStream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            HttpStream::Tls(s) => s.flush(),
            HttpStream::Tcp(s) => s.flush(),
        }
    }
}

/// Open the right stream for a parsed URL: TLS for https/wss, plain TCP otherwise.
fn connect_stream(
    host: &str,
    port: u16,
    use_tls: bool,
    skip_tls_verify: bool,
) -> Result<HttpStream> {
    if use_tls {
        Ok(HttpStream::Tls(EspTlsStream::connect(
            host,
            port,
            skip_tls_verify,
        )?))
    } else {
        Ok(HttpStream::Tcp(std::net::TcpStream::connect(format!(
            "{host}:{port}"
        ))?))
    }
}

fn build_raw_request(
    method: &str,
    path: &str,
    host: &str,
    headers: &std::collections::HashMap<String, String>,
    body: Option<&[u8]>,
) -> String {
    let mut raw = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        method, path, host
    );
    for (key, value) in headers {
        raw.push_str(&format!("{}: {}\r\n", key, value));
    }
    if let Some(body) = body {
        raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    raw.push_str("\r\n");
    raw
}

fn do_request(
    stream: &mut (impl std::io::Read + std::io::Write),
    raw_request: &str,
    body: Option<&[u8]>,
) -> Result<HttpResponse> {
    stream.write_all(raw_request.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    stream.flush()?;

    let mut response_buf = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response_buf.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.into()),
        }
    }

    parse_http_response(&response_buf)
}

fn do_streaming_request<S: std::io::Read + std::io::Write + Send + 'static>(
    mut stream: S,
    raw_request: &str,
) -> Result<StreamingHttpResponse> {
    stream.write_all(raw_request.as_bytes())?;
    stream.flush()?;

    // Read headers in chunks, scanning for \r\n\r\n delimiter
    let mut header_buf = Vec::with_capacity(1024);
    let mut buf = [0u8; 512];
    let header_end = loop {
        match stream.read(&mut buf) {
            Ok(0) => anyhow::bail!("Connection closed before headers complete"),
            Ok(n) => {
                header_buf.extend_from_slice(&buf[..n]);
                if let Some(pos) = header_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            }
            Err(e) => return Err(e.into()),
        }
    };

    let status_code = parse_status_code(&header_buf[..header_end])?;

    // Any bytes past the header delimiter were over-read body data
    let overflow = header_buf.split_off(header_end);
    let body: Box<dyn std::io::Read + Send> =
        Box::new(std::io::Cursor::new(overflow).chain(stream));

    Ok(StreamingHttpResponse { status_code, body })
}

fn parse_status_code(header_bytes: &[u8]) -> Result<u16> {
    let header_str = String::from_utf8_lossy(header_bytes);
    let status_line = header_str
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No status line"))?;
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("Malformed status line: {}", status_line))
}

fn parse_http_response(response_buf: &[u8]) -> Result<HttpResponse> {
    let header_end = response_buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("Malformed HTTP response"))?;

    let status_code = parse_status_code(&response_buf[..header_end])?;
    let body = response_buf[header_end + 4..].to_vec();

    Ok(HttpResponse { status_code, body })
}
