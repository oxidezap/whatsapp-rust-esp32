//! A WebSocket client that hands each frame over exactly once.
//!
//! This is the client half of RFC 6455, and only the part of it WhatsApp uses:
//! the HTTP upgrade, unfragmented and fragmented binary frames, ping/pong, and
//! close. It exists for one reason, and the reason is a number.
//!
//! On the ESP32-C3 the firmware aborted decoding a 28,204-byte server frame
//! with 48,028 bytes free, because the largest contiguous block was 20,480.
//! Two full-size copies of that frame were alive at once: the one a general
//! WebSocket library had grown its read buffer to hold -- and keeps for the
//! life of the connection, since the payload it hands out is a refcounted view
//! into that buffer -- and the one `whatsapp-rust`'s frame decoder made
//! because a view it does not own is all it was given. Reclaiming 12 KB of
//! stack widened everything except the block that mattered.
//!
//! So this reader allocates each binary payload once, at exactly its declared
//! size, reads the wire into it, and gives it away as a `Bytes` with **one
//! owner**. Nothing here retains it. A decoder that can take ownership
//! (`Bytes::try_into_mut`) then decrypts in place with no second copy at all;
//! one that cannot still copies from a buffer that dies the moment it has.
//!
//! The reader is a state machine driven by `read_frame`, which returns
//! `Ok(None)` on `WouldBlock`/`TimedOut` and resumes where it left off, so it
//! is safe to call from a poll loop over a socket with a short read timeout.
//! Everything is generic over `Read + Write` and the random source is injected,
//! which is what lets the codec be tested on a host without an ESP-IDF in it.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bytes::{Bytes, BytesMut};
use sha1::{Digest, Sha1};

/// RFC 6455 §1.3: the constant the server appends to the client key before
/// hashing it into `Sec-WebSocket-Accept`.
const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// A response header block larger than this is not a WebSocket server answering.
const MAX_HANDSHAKE_BYTES: usize = 4 * 1024;
/// Bound on waiting for the `101` after the request has been written.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Control frame payloads are capped by the protocol (§5.5).
const MAX_CONTROL_PAYLOAD: usize = 125;
/// The largest frame header: 2 bytes + 8-byte length. Server frames carry no
/// mask key, and a masked server frame is a protocol error here (§5.1).
const MAX_HEADER: usize = 10;
/// Outbound payloads are masked through a stack buffer this large, so sending
/// never allocates.
const MASK_CHUNK: usize = 256;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Upper bounds a peer can make this side allocate towards.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Largest single data frame accepted; the payload is allocated at this
    /// size at most, before any of it has arrived.
    pub max_frame_size: usize,
    /// Largest reassembled fragmented message accepted.
    pub max_message_size: usize,
}

/// One complete frame, or one reassembled message, from the server.
#[derive(Debug)]
pub enum Frame {
    /// A binary message. The `Bytes` has exactly one owner: the caller.
    Binary(Bytes),
    /// A text message; WhatsApp never sends one, but the protocol allows it.
    Text(Bytes),
    /// The server asked for a pong carrying this payload.
    Ping(Bytes),
    /// A pong; unsolicited ones are legal and ignored.
    Pong(Bytes),
    /// The server is closing, with the status code and reason if it sent them.
    Close(Option<(u16, String)>),
}

fn protocol_error(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("websocket: {what}"))
}

/// A frame whose header has been parsed and whose payload is still arriving.
struct Incoming {
    fin: bool,
    opcode: u8,
    payload: BytesMut,
    filled: usize,
}

/// A fragmented message being reassembled across continuation frames.
struct Partial {
    opcode: u8,
    buf: BytesMut,
}

pub struct WsClient<S> {
    stream: S,
    limits: Limits,
    fill_random: fn(&mut [u8]),
    /// Bytes the server sent after its `101` that were read along with it.
    /// They are the head of the first frame and are consumed before the
    /// stream is read again.
    pending: Vec<u8>,
    pending_pos: usize,
    header: [u8; MAX_HEADER],
    header_len: usize,
    incoming: Option<Incoming>,
    partial: Option<Partial>,
}

impl<S: Read + Write> WsClient<S> {
    /// Perform the HTTP upgrade on an already-connected stream.
    ///
    /// `fill_random` must be a real entropy source: it produces the
    /// `Sec-WebSocket-Key` and every outbound mask, and §10.3 exists because a
    /// predictable mask lets a hostile page poison caching proxies.
    pub fn connect(
        mut stream: S,
        host: &str,
        path: &str,
        origin: &str,
        limits: Limits,
        fill_random: fn(&mut [u8]),
    ) -> io::Result<Self> {
        let mut nonce = [0u8; 16];
        fill_random(&mut nonce);
        let key = BASE64.encode(nonce);

        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Origin: {origin}\r\n\
             \r\n"
        );
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let mut response = Vec::with_capacity(512);
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let header_end = loop {
            if let Some(end) = find_header_end(&response) {
                break end;
            }
            if response.len() >= MAX_HANDSHAKE_BYTES {
                return Err(protocol_error("handshake response too large"));
            }
            let mut chunk = [0u8; 256];
            match stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "websocket: connection closed during handshake",
                    ))
                }
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                Err(e) if is_would_block(&e) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "websocket: handshake timed out",
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        };

        let head = std::str::from_utf8(&response[..header_end])
            .map_err(|_| protocol_error("handshake response is not ASCII"))?;
        verify_upgrade(head, &key)?;

        // Whatever followed the blank line is the start of the first frame.
        let pending = response[header_end + 4..].to_vec();

        Ok(Self {
            stream,
            limits,
            fill_random,
            pending,
            pending_pos: 0,
            header: [0; MAX_HEADER],
            header_len: 0,
            incoming: None,
            partial: None,
        })
    }

    /// The underlying stream, for socket options.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Make progress on the next frame.
    ///
    /// Returns `Ok(None)` when the stream would block before a frame is
    /// complete; the partial header or payload is kept and the next call
    /// continues from it. A completed data frame is returned as its own
    /// message, or folded into the fragmented message it continues.
    pub fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        loop {
            if self.incoming.is_none() {
                match self.read_header()? {
                    Some(incoming) => self.incoming = Some(incoming),
                    None => return Ok(None),
                }
            }

            // Fill the payload. `incoming` is set by the arm above.
            let done = {
                let inc = self.incoming.as_mut().expect("incoming frame is set");
                if inc.filled < inc.payload.len() {
                    let mut filled = inc.filled;
                    let mut payload = std::mem::take(&mut inc.payload);
                    let outcome = Self::fill_into(
                        &mut self.stream,
                        &self.pending,
                        &mut self.pending_pos,
                        &mut payload,
                        &mut filled,
                    );
                    inc.payload = payload;
                    inc.filled = filled;
                    match outcome {
                        Ok(()) => {}
                        Err(e) if is_would_block(&e) => return Ok(None),
                        Err(e) => return Err(e),
                    }
                }
                inc.filled == inc.payload.len()
            };
            if !done {
                return Ok(None);
            }

            let inc = self.incoming.take().expect("incoming frame is set");
            if let Some(frame) = self.dispatch(inc)? {
                return Ok(Some(frame));
            }
        }
    }

    /// Send a binary message. Masks through a stack buffer; never allocates.
    pub fn send_binary(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send_frame(OP_BINARY, payload)
    }

    pub fn send_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send_frame(OP_PONG, payload)
    }

    /// Send a close frame with `code` and no reason.
    pub fn send_close(&mut self, code: u16) -> io::Result<()> {
        self.send_frame(OP_CLOSE, &code.to_be_bytes())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        if opcode >= OP_CLOSE && payload.len() > MAX_CONTROL_PAYLOAD {
            return Err(protocol_error("control frame payload over 125 bytes"));
        }

        let mut header = [0u8; 14];
        header[0] = 0x80 | opcode;
        let mut n = 2;
        let len = payload.len();
        if len <= 125 {
            header[1] = 0x80 | len as u8;
        } else if len <= u16::MAX as usize {
            header[1] = 0x80 | 126;
            header[2..4].copy_from_slice(&(len as u16).to_be_bytes());
            n = 4;
        } else {
            header[1] = 0x80 | 127;
            header[2..10].copy_from_slice(&(len as u64).to_be_bytes());
            n = 10;
        }
        let mut mask = [0u8; 4];
        (self.fill_random)(&mut mask);
        header[n..n + 4].copy_from_slice(&mask);
        n += 4;
        self.stream.write_all(&header[..n])?;

        let mut chunk = [0u8; MASK_CHUNK];
        for (i, block) in payload.chunks(MASK_CHUNK).enumerate() {
            let base = i * MASK_CHUNK;
            for (j, b) in block.iter().enumerate() {
                chunk[j] = b ^ mask[(base + j) & 3];
            }
            self.stream.write_all(&chunk[..block.len()])?;
        }
        self.stream.flush()
    }

    /// Read bytes: the handshake leftover first, then the stream.
    fn fill(
        stream: &mut S,
        pending: &[u8],
        pending_pos: &mut usize,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        let left = &pending[*pending_pos..];
        if !left.is_empty() {
            let n = left.len().min(buf.len());
            buf[..n].copy_from_slice(&left[..n]);
            *pending_pos += n;
            return Ok(n);
        }
        match stream.read(buf) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "websocket: connection closed",
            )),
            other => other,
        }
    }

    /// Fill `payload[*filled..]` until full or the stream would block.
    fn fill_into(
        stream: &mut S,
        pending: &[u8],
        pending_pos: &mut usize,
        payload: &mut BytesMut,
        filled: &mut usize,
    ) -> io::Result<()> {
        while *filled < payload.len() {
            let n = Self::fill(stream, pending, pending_pos, &mut payload[*filled..])?;
            *filled += n;
        }
        Ok(())
    }

    /// Accumulate and parse a frame header. `Ok(None)` means would-block.
    fn read_header(&mut self) -> io::Result<Option<Incoming>> {
        // The first two bytes say how long the rest of the header is.
        while self.header_len < 2 {
            match Self::fill(
                &mut self.stream,
                &self.pending,
                &mut self.pending_pos,
                &mut self.header[self.header_len..2],
            ) {
                Ok(n) => self.header_len += n,
                Err(e) if is_would_block(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }

        let b0 = self.header[0];
        let b1 = self.header[1];
        if b0 & 0x70 != 0 {
            return Err(protocol_error("reserved bits set"));
        }
        if b1 & 0x80 != 0 {
            // §5.1: a client MUST close the connection on a masked server frame.
            return Err(protocol_error("server frame is masked"));
        }
        let len7 = (b1 & 0x7F) as usize;
        let total = match len7 {
            126 => 4,
            127 => 10,
            _ => 2,
        };

        while self.header_len < total {
            match Self::fill(
                &mut self.stream,
                &self.pending,
                &mut self.pending_pos,
                &mut self.header[self.header_len..total],
            ) {
                Ok(n) => self.header_len += n,
                Err(e) if is_would_block(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        self.header_len = 0;

        let len = match len7 {
            126 => u16::from_be_bytes([self.header[2], self.header[3]]) as usize,
            127 => {
                let v = u64::from_be_bytes(self.header[2..10].try_into().expect("8 bytes"));
                usize::try_from(v).map_err(|_| protocol_error("frame length overflows usize"))?
            }
            n => n,
        };
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0F;

        match opcode {
            OP_CLOSE | OP_PING | OP_PONG => {
                if !fin {
                    return Err(protocol_error("fragmented control frame"));
                }
                if len > MAX_CONTROL_PAYLOAD {
                    return Err(protocol_error("control frame payload over 125 bytes"));
                }
            }
            OP_CONTINUATION => {
                let partial = self
                    .partial
                    .as_ref()
                    .ok_or_else(|| protocol_error("continuation frame without a message"))?;
                if partial.buf.len().saturating_add(len) > self.limits.max_message_size {
                    return Err(protocol_error("fragmented message over the size limit"));
                }
                if len > self.limits.max_frame_size {
                    return Err(protocol_error("frame over the size limit"));
                }
            }
            OP_TEXT | OP_BINARY => {
                if self.partial.is_some() {
                    return Err(protocol_error("new data frame inside a fragmented message"));
                }
                if len > self.limits.max_frame_size {
                    return Err(protocol_error("frame over the size limit"));
                }
                if !fin && len > self.limits.max_message_size {
                    return Err(protocol_error("fragmented message over the size limit"));
                }
            }
            _ => return Err(protocol_error("reserved opcode")),
        }

        // One allocation, exactly the declared size. This is the whole point.
        let mut payload = BytesMut::with_capacity(len);
        payload.resize(len, 0);

        Ok(Some(Incoming {
            fin,
            opcode,
            payload,
            filled: 0,
        }))
    }

    /// Turn a complete frame into what the caller sees, or fold it into the
    /// fragmented message in progress.
    fn dispatch(&mut self, inc: Incoming) -> io::Result<Option<Frame>> {
        let Incoming {
            fin,
            opcode,
            payload,
            ..
        } = inc;
        Ok(Some(match opcode {
            OP_PING => Frame::Ping(payload.freeze()),
            OP_PONG => Frame::Pong(payload.freeze()),
            OP_CLOSE => Frame::Close(parse_close(&payload)?),
            OP_CONTINUATION => {
                let mut partial = self.partial.take().expect("checked in read_header");
                partial.buf.extend_from_slice(&payload);
                drop(payload);
                if fin {
                    let data = partial.buf.freeze();
                    match partial.opcode {
                        OP_TEXT => Frame::Text(data),
                        _ => Frame::Binary(data),
                    }
                } else {
                    self.partial = Some(partial);
                    return Ok(None);
                }
            }
            OP_TEXT | OP_BINARY if fin => {
                let data = payload.freeze();
                if opcode == OP_TEXT {
                    Frame::Text(data)
                } else {
                    Frame::Binary(data)
                }
            }
            OP_TEXT | OP_BINARY => {
                // First fragment: its buffer becomes the accumulator.
                self.partial = Some(Partial {
                    opcode,
                    buf: payload,
                });
                return Ok(None);
            }
            _ => unreachable!("read_header rejects reserved opcodes"),
        }))
    }
}

fn is_would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The value the server must echo back for `key` (§4.2.2 step 5.4).
fn expected_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(ACCEPT_GUID);
    BASE64.encode(hasher.finalize())
}

/// Check the status line and the three headers the upgrade depends on.
fn verify_upgrade(head: &str, key: &str) -> io::Result<()> {
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let code = status
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .ok_or_else(|| protocol_error("malformed status line"))?;
    if code != "101" {
        return Err(protocol_error(&format!("upgrade refused: {status}")));
    }

    let mut upgrade_ok = false;
    let mut connection_ok = false;
    let mut accept: Option<&str> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("upgrade") {
            upgrade_ok = value.eq_ignore_ascii_case("websocket");
        } else if name.eq_ignore_ascii_case("connection") {
            connection_ok = value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
        } else if name.eq_ignore_ascii_case("sec-websocket-accept") {
            accept = Some(value);
        }
    }

    if !upgrade_ok {
        return Err(protocol_error("missing `Upgrade: websocket`"));
    }
    if !connection_ok {
        return Err(protocol_error("missing `Connection: Upgrade`"));
    }
    match accept {
        Some(got) if got == expected_accept(key) => Ok(()),
        Some(_) => Err(protocol_error("Sec-WebSocket-Accept does not match")),
        None => Err(protocol_error("missing Sec-WebSocket-Accept")),
    }
}

/// §5.5.1: an empty body, or a 2-byte code followed by a UTF-8 reason.
fn parse_close(payload: &[u8]) -> io::Result<Option<(u16, String)>> {
    match payload.len() {
        0 => Ok(None),
        1 => Err(protocol_error("close frame with a 1-byte payload")),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            let reason = std::str::from_utf8(&payload[2..])
                .map_err(|_| protocol_error("close reason is not UTF-8"))?;
            Ok(Some((code, reason.to_owned())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;

    /// A scripted peer: reads come from `inbound` in the exact chunks given
    /// (so partial reads and would-blocks are reproducible), writes land in
    /// `outbound`.
    struct Script {
        inbound: VecDeque<io::Result<Vec<u8>>>,
        outbound: Vec<u8>,
    }

    impl Script {
        fn new() -> Self {
            Self {
                inbound: VecDeque::new(),
                outbound: Vec::new(),
            }
        }
        fn chunk(mut self, bytes: &[u8]) -> Self {
            self.inbound.push_back(Ok(bytes.to_vec()));
            self
        }
        fn would_block(mut self) -> Self {
            self.inbound
                .push_back(Err(io::Error::from(io::ErrorKind::WouldBlock)));
            self
        }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.inbound.pop_front() {
                Some(Ok(chunk)) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        self.inbound.push_front(Ok(chunk[n..].to_vec()));
                    }
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => Ok(0),
            }
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.outbound.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn zero_random(buf: &mut [u8]) {
        buf.fill(0);
    }

    fn limits() -> Limits {
        Limits {
            max_frame_size: 32 * 1024,
            max_message_size: 32 * 1024,
        }
    }

    /// The key a zeroed nonce produces, and the accept that answers it.
    const ZERO_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAA==";

    fn upgrade_response(extra: &[u8]) -> Vec<u8> {
        let mut v = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\
             \r\n",
            expected_accept(ZERO_KEY)
        )
        .into_bytes();
        v.extend_from_slice(extra);
        v
    }

    /// An unmasked server frame, as the server would send it.
    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(if fin { 0x80 } else { 0 }) | opcode];
        let len = payload.len();
        if len <= 125 {
            v.push(len as u8);
        } else if len <= 0xFFFF {
            v.push(126);
            v.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            v.push(127);
            v.extend_from_slice(&(len as u64).to_be_bytes());
        }
        v.extend_from_slice(payload);
        v
    }

    fn connect(script: Script) -> WsClient<Script> {
        WsClient::connect(
            script,
            "example.test",
            "/ws",
            "https://example.test",
            limits(),
            zero_random,
        )
        .expect("handshake")
    }

    #[test]
    fn accept_value_matches_rfc_example() {
        // RFC 6455 §1.3 worked example.
        assert_eq!(
            expected_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn handshake_sends_a_well_formed_request_and_accepts_101() {
        let ws = connect(Script::new().chunk(&upgrade_response(b"")));
        let req = String::from_utf8(ws.stream.outbound.clone()).unwrap();
        assert!(req.starts_with("GET /ws HTTP/1.1\r\n"));
        assert!(req.contains("Host: example.test\r\n"));
        assert!(req.contains("Upgrade: websocket\r\n"));
        assert!(req.contains(&format!("Sec-WebSocket-Key: {ZERO_KEY}\r\n")));
        assert!(req.contains("Sec-WebSocket-Version: 13\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn handshake_rejects_a_wrong_accept() {
        let bad = upgrade_response(b"").replace_accept("nope");
        let err = WsClient::connect(
            Script::new().chunk(&bad),
            "h",
            "/",
            "o",
            limits(),
            zero_random,
        )
        .err()
        .expect("must fail");
        assert!(err.to_string().contains("Sec-WebSocket-Accept"));
    }

    #[test]
    fn handshake_rejects_a_non_101() {
        let resp = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
        let err = WsClient::connect(
            Script::new().chunk(resp),
            "h",
            "/",
            "o",
            limits(),
            zero_random,
        )
        .err()
        .expect("must fail");
        assert!(err.to_string().contains("403"));
    }

    #[test]
    fn bytes_after_the_upgrade_are_the_first_frame() {
        let frame = server_frame(true, OP_BINARY, b"hello");
        let mut ws = connect(Script::new().chunk(&upgrade_response(&frame)));
        match ws.read_frame().unwrap() {
            Some(Frame::Binary(b)) => assert_eq!(&b[..], b"hello"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_frame_split_across_reads_and_would_blocks_is_reassembled() {
        let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let frame = server_frame(true, OP_BINARY, &payload);
        // Split inside the header, then inside the payload, with a would-block
        // between every piece.
        let script = Script::new()
            .chunk(&upgrade_response(b""))
            .chunk(&frame[..1])
            .would_block()
            .chunk(&frame[1..3])
            .would_block()
            .chunk(&frame[3..100])
            .would_block()
            .chunk(&frame[100..]);
        let mut ws = connect(script);
        assert!(ws.read_frame().unwrap().is_none());
        assert!(ws.read_frame().unwrap().is_none());
        assert!(ws.read_frame().unwrap().is_none());
        match ws.read_frame().unwrap() {
            Some(Frame::Binary(b)) => assert_eq!(&b[..], &payload[..]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_binary_payload_is_handed_over_with_exactly_one_owner() {
        let payload = vec![7u8; 28_204];
        let frame = server_frame(true, OP_BINARY, &payload);
        let mut ws = connect(Script::new().chunk(&upgrade_response(b"")).chunk(&frame));
        let Some(Frame::Binary(bytes)) = ws.read_frame().unwrap() else {
            panic!("expected binary");
        };
        // Unique ownership is what lets a decoder decrypt in place.
        let owned = bytes.try_into_mut().expect("no other reference exists");
        assert_eq!(owned.len(), 28_204);
        assert_eq!(
            owned.capacity(),
            28_204,
            "allocated at exactly the declared size"
        );
    }

    #[test]
    fn fragments_are_reassembled_and_control_frames_may_interleave() {
        let script = Script::new()
            .chunk(&upgrade_response(b""))
            .chunk(&server_frame(false, OP_BINARY, b"ab"))
            .chunk(&server_frame(true, OP_PING, b"p"))
            .chunk(&server_frame(false, OP_CONTINUATION, b"cd"))
            .chunk(&server_frame(true, OP_CONTINUATION, b"e"));
        let mut ws = connect(script);
        match ws.read_frame().unwrap() {
            Some(Frame::Ping(p)) => assert_eq!(&p[..], b"p"),
            other => panic!("unexpected {other:?}"),
        }
        match ws.read_frame().unwrap() {
            Some(Frame::Binary(b)) => assert_eq!(&b[..], b"abcde"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn close_frame_is_parsed() {
        let mut body = 1001u16.to_be_bytes().to_vec();
        body.extend_from_slice(b"going away");
        let mut ws = connect(
            Script::new()
                .chunk(&upgrade_response(b""))
                .chunk(&server_frame(true, OP_CLOSE, &body)),
        );
        match ws.read_frame().unwrap() {
            Some(Frame::Close(Some((1001, r)))) => assert_eq!(r, "going away"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn masked_server_frames_and_oversize_frames_are_protocol_errors() {
        let mut masked = server_frame(true, OP_BINARY, b"x");
        masked[1] |= 0x80;
        let mut ws = connect(Script::new().chunk(&upgrade_response(b"")).chunk(&masked));
        assert!(ws.read_frame().unwrap_err().to_string().contains("masked"));

        let big = server_frame(true, OP_BINARY, &vec![0u8; 32 * 1024 + 1]);
        let mut ws = connect(
            Script::new()
                .chunk(&upgrade_response(b""))
                .chunk(&big[..12]),
        );
        // The header alone is enough to refuse it: nothing is allocated.
        assert!(ws
            .read_frame()
            .unwrap_err()
            .to_string()
            .contains("size limit"));
    }

    #[test]
    fn fragmented_message_over_the_limit_is_refused_before_the_last_fragment() {
        let half = vec![0u8; 20 * 1024];
        let script = Script::new()
            .chunk(&upgrade_response(b""))
            .chunk(&server_frame(false, OP_BINARY, &half))
            .chunk(&server_frame(true, OP_CONTINUATION, &half)[..12]);
        let mut ws = connect(script);
        // The first fragment produces no frame, so the same call goes on to
        // the next header and refuses it there: one call, one error, and the
        // second 20 KB is never allocated.
        assert!(ws
            .read_frame()
            .unwrap_err()
            .to_string()
            .contains("size limit"));
    }

    #[test]
    fn outbound_frames_are_masked_with_a_final_bit_and_the_right_length_form() {
        let mut ws = connect(Script::new().chunk(&upgrade_response(b"")));
        let mark = ws.stream.outbound.len();
        ws.send_binary(&[1, 2, 3]).unwrap();
        // Mask is all zeros under `zero_random`, so payload bytes pass through.
        assert_eq!(
            &ws.stream.outbound[mark..],
            &[0x82, 0x83, 0, 0, 0, 0, 1, 2, 3]
        );

        let mark = ws.stream.outbound.len();
        ws.send_binary(&[9u8; 300]).unwrap();
        let out = &ws.stream.outbound[mark..];
        assert_eq!(&out[..4], &[0x82, 0x80 | 126, 0x01, 0x2C]);
        assert_eq!(out.len(), 2 + 2 + 4 + 300);

        let mark = ws.stream.outbound.len();
        ws.send_close(1000).unwrap();
        assert_eq!(
            &ws.stream.outbound[mark..],
            &[0x88, 0x82, 0, 0, 0, 0, 0x03, 0xE8]
        );
    }

    #[test]
    fn masking_applies_the_key_cyclically_across_chunks() {
        fn fixed_random(buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = [0x11, 0x22, 0x33, 0x44][i & 3];
            }
        }
        let mut ws = WsClient::connect(
            Script::new().chunk(&{
                let mut v = format!(
                    "HTTP/1.1 101 x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                    expected_accept(&BASE64.encode({
                        let mut n = [0u8; 16];
                        fixed_random(&mut n);
                        n
                    }))
                )
                .into_bytes();
                v.truncate(v.len());
                v
            }),
            "h",
            "/",
            "o",
            limits(),
            fixed_random,
        )
        .expect("handshake");
        let mark = ws.stream.outbound.len();
        let payload: Vec<u8> = (0..600u32).map(|i| (i * 7) as u8).collect();
        ws.send_binary(&payload).unwrap();
        let out = &ws.stream.outbound[mark..];
        let mask = &out[4..8];
        assert_eq!(mask, &[0x11, 0x22, 0x33, 0x44]);
        for (i, b) in out[8..].iter().enumerate() {
            assert_eq!(*b, payload[i] ^ mask[i & 3], "byte {i}");
        }
    }

    trait ReplaceAccept {
        fn replace_accept(self, with: &str) -> Vec<u8>;
    }
    impl ReplaceAccept for Vec<u8> {
        fn replace_accept(self, with: &str) -> Vec<u8> {
            let s = String::from_utf8(self).unwrap();
            let good = expected_accept(ZERO_KEY);
            s.replace(&good, with).into_bytes()
        }
    }
}
