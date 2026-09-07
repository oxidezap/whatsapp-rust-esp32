use std::io::{self, BufRead, BufReader, Read};

const MAX_METADATA_BYTES: u64 = 8 * 1024;

pub(crate) struct ChunkedReader<R> {
    inner: BufReader<R>,
    remaining: u64,
    needs_crlf: bool,
    finished: bool,
}

impl<R: Read> ChunkedReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner: BufReader::with_capacity(512, inner),
            remaining: 0,
            needs_crlf: false,
            finished: false,
        }
    }

    fn line(&mut self, limit: u64) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        self.inner
            .by_ref()
            .take(limit)
            .read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\r\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Truncated or oversized HTTP chunk metadata",
            ));
        }
        Ok(line)
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.finished {
            return Ok(0);
        }
        if self.remaining == 0 {
            if self.needs_crlf {
                let mut crlf = [0; 2];
                self.inner.read_exact(&mut crlf)?;
                if crlf != *b"\r\n" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Invalid chunk terminator",
                    ));
                }
            }
            let line = self.line(MAX_METADATA_BYTES)?;
            let size = line[..line.len() - 2].split(|b| *b == b';').next().unwrap();
            if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid HTTP chunk size",
                ));
            }
            self.remaining = u64::from_str_radix(std::str::from_utf8(size).unwrap(), 16)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if self.remaining == 0 {
                let mut budget = MAX_METADATA_BYTES;
                loop {
                    let trailer = self.line(budget)?;
                    budget -= trailer.len() as u64;
                    if trailer == b"\r\n" {
                        self.finished = true;
                        return Ok(0);
                    }
                    if !trailer[..trailer.len() - 2].contains(&b':') {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Invalid HTTP trailer",
                        ));
                    }
                }
            }
            self.needs_crlf = true;
        }
        let count = self.remaining.min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..count])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Truncated HTTP chunk",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_extensions_trailers_and_short_reads() {
        let input = b"4;foo=bar\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Checksum: ignored\r\n\r\n";
        let mut reader = ChunkedReader::new(&input[..]);
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let mut result = Vec::new();
        let mut buf = [0; 2];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&buf[..n]);
        }
        assert_eq!(result, b"Wikipedia");
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn every_truncation_is_an_error() {
        let input = b"3\r\nabc\r\n0\r\nX-Trailer: yes\r\n\r\n";
        for end in 0..input.len() {
            assert!(
                ChunkedReader::new(&input[..end])
                    .read_to_end(&mut Vec::new())
                    .is_err(),
                "end={end}"
            );
        }
    }

    #[test]
    fn rejects_invalid_sizes_and_framing() {
        for input in [
            &b"+1\r\na\r\n0\r\n\r\n"[..],
            b" 1\r\na\r\n0\r\n\r\n",
            b"g\r\n",
            b"10000000000000000\r\n",
            b"1\r\naXX0\r\n\r\n",
            b"0\r\ninvalid\r\n\r\n",
        ] {
            assert!(ChunkedReader::new(input)
                .read_to_end(&mut Vec::new())
                .is_err());
        }
    }

    #[test]
    fn bounds_metadata_not_chunk_payload() {
        let oversized = vec![b'a'; MAX_METADATA_BYTES as usize + 1];
        assert!(ChunkedReader::new(oversized.as_slice())
            .read_to_end(&mut Vec::new())
            .is_err());
        let mut input = b"0\r\n".to_vec();
        for _ in 0..2048 {
            input.extend_from_slice(b"X: y\r\n");
        }
        assert!(ChunkedReader::new(input.as_slice())
            .read_to_end(&mut Vec::new())
            .is_err());
        let mut input = b"3000\r\n".to_vec();
        input.extend_from_slice(&vec![b'x'; 0x3000]);
        input.extend_from_slice(b"\r\n0\r\n\r\n");
        let mut body = Vec::new();
        ChunkedReader::new(input.as_slice())
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body.len(), 0x3000);
    }

    #[test]
    fn terminal_chunk_does_not_wait_for_socket_close() {
        struct OpenSocket(io::Cursor<Vec<u8>>);
        impl Read for OpenSocket {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    panic!("read beyond complete response");
                }
                self.0.read(buf)
            }
        }
        let mut body = Vec::new();
        ChunkedReader::new(OpenSocket(io::Cursor::new(b"0\r\n\r\n".to_vec())))
            .read_to_end(&mut body)
            .unwrap();
        assert!(body.is_empty());
    }
}
