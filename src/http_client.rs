use std::io::{Read, Write};
use std::net::ToSocketAddrs;

use anyhow::Result;
use whatsapp_rust::async_trait;
use whatsapp_rust::wacore::net::{HttpClient, HttpRequest, HttpResponse, StreamingHttpResponse};

use crate::transport::EspTlsStream;

/// Socket-level bound on every media/version request. Without one a peer that
/// accepts the connection and then goes quiet holds the calling thread forever;
/// with the blocking download path that is the executor.
const HTTP_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// A response header block past this is not a WhatsApp CDN answering.
const MAX_RESPONSE_HEADER_BYTES: usize = 8 * 1024;
/// Maximum response size for non-streaming buffered HTTP requests.
const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

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
        )?;

        let dial = qemu_host(host);
        let mut stream = connect_stream(&dial, port, use_tls, self.skip_tls_verify)?;
        do_request(&mut stream, &raw_request, request.body.as_deref())
    }

    /// Advertised so upstream streams media straight into its decryptor instead
    /// of buffering a whole download, which on this chip means PSRAM, not RAM.
    fn supports_streaming(&self) -> bool {
        true
    }

    fn execute_streaming(&self, request: HttpRequest) -> Result<StreamingHttpResponse> {
        let (host, port, path, use_tls) = parse_url(&request.url)?;
        let raw_request = build_raw_request(&request.method, &path, &host, &request.headers, None)?;

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

impl HttpStream {
    fn set_timeouts(&self) -> Result<()> {
        match self {
            Self::Tls(stream) => {
                let ms = HTTP_IO_TIMEOUT.as_millis() as u32;
                stream.set_read_timeout_ms(ms)?;
                stream.set_write_timeout_ms(ms)
            }
            Self::Tcp(stream) => {
                stream.set_read_timeout(Some(HTTP_IO_TIMEOUT))?;
                stream.set_write_timeout(Some(HTTP_IO_TIMEOUT))?;
                Ok(())
            }
        }
    }
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
    let stream = if use_tls {
        HttpStream::Tls(EspTlsStream::connect_with_timeout(
            host,
            port,
            skip_tls_verify,
            HTTP_IO_TIMEOUT,
        )?)
    } else {
        // `TcpStream::connect` has no deadline of its own, so resolve first and
        // dial each candidate with one: preserve fallback across multiple resolved
        // addresses (e.g. IPv4/IPv6 or multi-homed hosts).
        let addrs = format!("{host}:{port}").to_socket_addrs()?;
        let mut last_err = None;
        let mut stream = None;
        for address in addrs {
            match std::net::TcpStream::connect_timeout(&address, HTTP_IO_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        let stream = match (stream, last_err) {
            (Some(s), _) => s,
            (None, Some(e)) => return Err(e.into()),
            (None, None) => anyhow::bail!("{host}:{port} did not resolve to any addresses"),
        };
        HttpStream::Tcp(stream)
    };
    stream.set_timeouts()?;
    Ok(stream)
}

fn build_raw_request(
    method: &str,
    path: &str,
    host: &str,
    headers: &std::collections::HashMap<String, String>,
    body: Option<&[u8]>,
) -> Result<String> {
    // The request line and headers are assembled by hand, so anything with a
    // CR/LF in it would become a second header (or a second request). Every
    // component comes from upstream or the server's own URLs, but the check is
    // cheap and the failure mode is silent.
    if method.is_empty() || !method.bytes().all(is_http_token_byte) {
        anyhow::bail!("Invalid HTTP method");
    }
    if !path.starts_with('/')
        || path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        anyhow::bail!("Invalid HTTP request target");
    }
    if host.is_empty()
        || host
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        anyhow::bail!("Invalid HTTP host");
    }
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (key, value) in headers {
        if key.is_empty()
            || !key.bytes().all(is_http_token_byte)
            || value
                .bytes()
                .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
        {
            anyhow::bail!("Invalid HTTP header");
        }
        // The body's real length is written below; a caller-supplied one would
        // duplicate (or contradict) it.
        if body.is_some() && key.eq_ignore_ascii_case("content-length") {
            continue;
        }
        raw.push_str(&format!("{key}: {value}\r\n"));
    }
    if let Some(body) = body {
        raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    raw.push_str("\r\n");
    Ok(raw)
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
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

    // The response ends at EOF, which `Connection: close` above guarantees.
    // Both streams are blocking with the timeouts `set_timeouts` installed, so a
    // WouldBlock/TimedOut here is that timeout firing, never a finished body:
    // treating it as EOF would hand back a silently truncated response, and for
    // a media download that is a corrupt file reported as a success.
    let mut response_buf = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if response_buf.len() + n > MAX_RESPONSE_BODY_BYTES {
                    anyhow::bail!(
                        "HTTP response body exceeds size limit of {MAX_RESPONSE_BODY_BYTES} bytes"
                    );
                }
                response_buf.extend_from_slice(&buf[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                anyhow::bail!("HTTP response timed out after {HTTP_IO_TIMEOUT:?}");
            }
            Err(e) => return Err(e.into()),
        }
    }

    parse_http_response(&response_buf)
}

/// A reader wrapper that enforces an exact byte count: if the underlying stream
/// ends before `remaining` bytes are read, it returns `UnexpectedEof` rather than
/// a truncated successful `Ok(0)`.
struct ExactLengthReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for ExactLengthReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.remaining == 0 {
            return Ok(0);
        }
        let max_to_read = self.remaining.min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..max_to_read])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "Connection closed prematurely: {} bytes remaining",
                    self.remaining
                ),
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
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
                if header_buf.len() > MAX_RESPONSE_HEADER_BYTES {
                    anyhow::bail!("HTTP response headers exceed size limit");
                }
            }
            Err(e) => return Err(e.into()),
        }
    };

    let status_code = parse_status_code(&header_buf[..header_end])?;
    let content_length = parse_streaming_body_length(&header_buf[..header_end])?;

    // Any bytes past the header delimiter were over-read body data. When the
    // response declares a length the reader stops there, so a decrypt-as-you-go
    // download cannot be left waiting on a socket that never closes. Without one
    // the body is delimited by the close that `Connection: close` asks for, and
    // reading to EOF is the only framing available.
    let overflow = header_buf.split_off(header_end);
    let rest = std::io::Cursor::new(overflow).chain(stream);
    let body: Box<dyn std::io::Read + Send> = match content_length {
        Some(length) => Box::new(ExactLengthReader {
            inner: rest,
            remaining: length,
        }),
        None => Box::new(rest),
    };

    Ok(StreamingHttpResponse { status_code, body })
}

/// The declared body length, or `None` when the response is delimited by the
/// connection closing. A transfer encoding we cannot decode is an error rather
/// than a `None`: handing the framed bytes back as the body would corrupt the
/// download and report it as a success.
fn parse_streaming_body_length(header_bytes: &[u8]) -> Result<Option<u64>> {
    let header = std::str::from_utf8(header_bytes)?;
    let mut content_length = None;
    for line in header.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().eq_ignore_ascii_case("identity")
        {
            anyhow::bail!("Unsupported HTTP transfer encoding: {}", value.trim());
        }
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed = value.trim().parse::<u64>()?;
            if content_length.replace(parsed).is_some() {
                anyhow::bail!("Duplicate HTTP Content-Length header");
            }
        }
    }
    Ok(content_length)
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
    let content_length = parse_streaming_body_length(&response_buf[..header_end])?;
    let body_slice = &response_buf[header_end + 4..];
    if let Some(expected_len) = content_length {
        let expected_usize = usize::try_from(expected_len)
            .map_err(|_| anyhow::anyhow!("Content-Length {expected_len} exceeds address space"))?;
        if body_slice.len() < expected_usize {
            anyhow::bail!(
                "HTTP response truncated: expected {expected_usize} bytes, got {}",
                body_slice.len()
            );
        }
        return Ok(HttpResponse {
            status_code,
            body: body_slice[..expected_usize].to_vec(),
        });
    }

    Ok(HttpResponse {
        status_code,
        body: body_slice.to_vec(),
    })
}
