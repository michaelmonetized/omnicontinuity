use crate::{Result, protocol};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    net::SocketAddrV6,
};

const HEAD_LIMIT: usize = 32 * 1024;
const LINE_LIMIT: usize = 8192;
const COMMON: &str = "Connection: keep-alive\r\nAccept: */*\r\nUser-Agent: AirDrop/1.0\r\nAccept-Language: en-us\r\nAccept-Encoding: br, gzip, deflate\r\n";

pub(crate) struct Head {
    pub first: String,
    headers: BTreeMap<String, String>,
}
pub(crate) struct Message {
    pub head: Head,
    pub body: Vec<u8>,
}
impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
    pub fn request_path(&self) -> Result<String> {
        let words: Vec<_> = self.first.split_whitespace().collect();
        if words.len() != 3 || words[0] != "POST" || words[2] != "HTTP/1.1" {
            return Err(protocol("expected HTTP/1.1 POST"));
        }
        Ok(words[1].into())
    }
}
impl Message {
    pub fn status(&self) -> Result<u16> {
        let mut words = self.head.first.split_whitespace();
        if !matches!(words.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
            return Err(protocol("invalid HTTP response"));
        }
        words
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| protocol("missing HTTP status"))
    }
}

fn line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if bytes.len() > limit || !bytes.ends_with(b"\r\n") {
        return Err(protocol("truncated or oversized HTTP line"));
    }
    bytes.truncate(bytes.len() - 2);
    Ok(bytes)
}

pub(crate) fn read_head(reader: &mut impl BufRead) -> Result<Head> {
    let first = String::from_utf8(line(reader, LINE_LIMIT)?)
        .map_err(|_| protocol("invalid HTTP start line"))?;
    let mut headers = BTreeMap::new();
    let mut count = first.len();
    loop {
        let bytes = line(reader, LINE_LIMIT)?;
        count += bytes.len() + 2;
        if count > HEAD_LIMIT {
            return Err(protocol("HTTP headers exceed limit"));
        }
        if bytes.is_empty() {
            break;
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| protocol("invalid HTTP header"))?;
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| protocol("HTTP header has no colon"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&v))
            || value.bytes().any(|v| v < 0x20 && v != b'\t')
        {
            return Err(protocol("invalid HTTP header"));
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err(protocol("duplicate HTTP header"));
        }
    }
    Ok(Head { first, headers })
}

pub(crate) fn read_body(
    reader: &mut impl BufRead,
    head: &Head,
    out: &mut impl Write,
    limit: u64,
) -> Result<()> {
    validate_framing(head, limit)?;
    let length = head.header("content-length");
    let chunked = head.header("transfer-encoding");
    if length.is_some() && chunked.is_some() {
        return Err(protocol("ambiguous HTTP body framing"));
    }
    match (length, chunked) {
        (Some(length), None) => {
            let size = length
                .parse::<u64>()
                .map_err(|_| protocol("invalid Content-Length"))?;
            if size > limit {
                return Err(protocol("HTTP body exceeds limit"));
            }
            copy_exact(reader, out, size)?;
        }
        (None, Some(encoding)) if encoding.eq_ignore_ascii_case("chunked") => {
            let mut total = 0u64;
            let mut chunks = 0u64;
            loop {
                let raw = line(reader, LINE_LIMIT)?;
                let size = std::str::from_utf8(&raw)
                    .ok()
                    .and_then(|v| v.split(';').next())
                    .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|v| u64::from_str_radix(v, 16).ok())
                    .ok_or_else(|| protocol("invalid HTTP chunk size"))?;
                chunks += 1;
                if chunks > 1_000_000 {
                    return Err(protocol("too many HTTP chunks"));
                }
                total = total
                    .checked_add(size)
                    .filter(|&v| v <= limit)
                    .ok_or_else(|| protocol("HTTP body exceeds limit"))?;
                if size == 0 {
                    let mut trailers = 0;
                    loop {
                        let trailer = line(reader, LINE_LIMIT)?;
                        trailers += trailer.len() + 2;
                        if trailers > HEAD_LIMIT {
                            return Err(protocol("HTTP trailers exceed limit"));
                        }
                        if trailer.is_empty() {
                            break;
                        }
                    }
                    break;
                }
                copy_exact(reader, out, size)?;
                if !line(reader, 2)?.is_empty() {
                    return Err(protocol("chunk lacks CRLF"));
                }
            }
        }
        _ => return Err(protocol("body needs Content-Length or chunked encoding")),
    }
    Ok(())
}

pub(crate) fn validate_framing(head: &Head, limit: u64) -> Result<()> {
    match (
        head.header("content-length"),
        head.header("transfer-encoding"),
    ) {
        (Some(length), None) if length.parse::<u64>().is_ok_and(|size| size <= limit) => Ok(()),
        (None, Some(encoding)) if encoding.eq_ignore_ascii_case("chunked") => Ok(()),
        _ => Err(protocol(
            "invalid, ambiguous or oversized HTTP body framing",
        )),
    }
}

pub(crate) fn continue_request(stream: &mut impl Write, head: &Head) -> Result<()> {
    if let Some(expect) = head.header("expect") {
        if !expect.eq_ignore_ascii_case("100-continue") {
            return Err(protocol("unsupported HTTP expectation"));
        }
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        stream.flush()?;
    }
    Ok(())
}

pub(crate) fn read_message(reader: &mut impl BufRead, limit: u64) -> Result<Message> {
    for _ in 0..8 {
        let head = read_head(reader)?;
        let mut message = Message { head, body: vec![] };
        let status = message.status()?;
        if (100..200).contains(&status) {
            continue;
        }
        if message
            .head
            .header("content-encoding")
            .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
        {
            return Err(protocol("compressed HTTP response is unsupported"));
        }
        read_body(reader, &message.head, &mut message.body, limit)?;
        return Ok(message);
    }
    Err(protocol("too many informational responses"))
}

pub(crate) fn copy_exact(input: &mut impl Read, output: &mut impl Write, size: u64) -> Result<()> {
    let copied = std::io::copy(&mut input.take(size), output)?;
    if copied != size {
        return Err(protocol("truncated body or file changed during transfer"));
    }
    Ok(())
}

pub(crate) fn post(
    stream: &mut impl Write,
    path: &str,
    peer: &SocketAddrV6,
    body: &[u8],
) -> Result<()> {
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: [{}]:{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n{COMMON}\r\n",
        peer.ip(),
        peer.port(),
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

pub(crate) fn upload_headers(
    stream: &mut impl Write,
    peer: &SocketAddrV6,
    total: u64,
    transfer: &str,
) -> Result<()> {
    write!(
        stream,
        "POST /Upload HTTP/1.1\r\nHost: [{}]:{}\r\nTransfer-Encoding: chunked\r\nContent-Type: application/x-dvzip\r\n{COMMON}TotalBytes: {total}\r\nTransferID: {transfer}\r\n\r\n",
        peer.ip(),
        peer.port()
    )?;
    Ok(())
}

pub(crate) fn answer(stream: &mut impl Write, status: u16, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Content",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

pub(crate) struct ChunkedWriter<W> {
    output: W,
}
impl<W: Write> ChunkedWriter<W> {
    pub fn new(output: W) -> Self {
        Self { output }
    }
    pub fn finish(mut self) -> Result<W> {
        self.output.write_all(b"0\r\n\r\n")?;
        self.output.flush()?;
        Ok(self.output)
    }
}
impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !bytes.is_empty() {
            write!(self.output, "{:X}\r\n", bytes.len())?;
            self.output.write_all(bytes)?;
            self.output.write_all(b"\r\n")?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
