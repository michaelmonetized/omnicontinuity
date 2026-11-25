use crate::invalid;
use std::collections::BTreeMap;
use std::io;

const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Request {
    pub method: String,
    pub target: String,
    pub protocol: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn path(&self) -> &str {
        let target = if let Some((_, rest)) = self.target.split_once("://") {
            rest.find('/').map_or("/", |index| &rest[index..])
        } else {
            &self.target
        };
        target.split('?').next().unwrap_or(target)
    }
}

#[derive(Default)]
pub(crate) struct Framer {
    pending: Vec<u8>,
}

impl Framer {
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.pending.len() + bytes.len() > MAX_HEADERS + MAX_BODY + 8192 {
            return Err(invalid("RTSP buffer limit exceeded"));
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn finish(&self) -> io::Result<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(invalid("Control connection closed during an RTSP message"))
        }
    }

    pub fn next(&mut self) -> io::Result<Option<Request>> {
        let Some(end) = self
            .pending
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
        else {
            if self.pending.len() > MAX_HEADERS {
                return Err(invalid("RTSP headers too large"));
            }
            return Ok(None);
        };
        if end + 4 > MAX_HEADERS {
            return Err(invalid("RTSP headers too large"));
        }
        let text = std::str::from_utf8(&self.pending[..end])
            .map_err(|_| invalid("RTSP header is not UTF-8"))?;
        let mut lines = text.split("\r\n");
        let first: Vec<_> = lines.next().unwrap_or_default().split(' ').collect();
        if first.len() != 3
            || first[0].is_empty()
            || first[1].is_empty()
            || !matches!(first[2], "RTSP/1.0" | "HTTP/1.1" | "HTTP/1.0")
        {
            return Err(invalid("Invalid RTSP request line"));
        }
        let mut headers = BTreeMap::new();
        for line in lines {
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| invalid("Invalid RTSP header"))?;
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                || value.bytes().any(|byte| byte < 32 && byte != b'\t')
            {
                return Err(invalid("Invalid RTSP header characters"));
            }
            if headers
                .insert(key.to_ascii_lowercase(), value.trim().to_owned())
                .is_some()
            {
                return Err(invalid("Duplicate RTSP header"));
            }
        }
        if headers.contains_key("transfer-encoding") {
            return Err(invalid("Chunked RTSP bodies are unsupported"));
        }
        let size = match headers.get("content-length") {
            Some(value) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                value
                    .parse::<usize>()
                    .map_err(|_| invalid("Invalid Content-Length"))?
            }
            Some(_) => return Err(invalid("Invalid Content-Length")),
            None => 0,
        };
        if size > MAX_BODY {
            return Err(invalid("RTSP body too large"));
        }
        if let Some(cseq) = headers.get("cseq")
            && (cseq.is_empty() || !cseq.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(invalid("Invalid CSeq"));
        }
        if self.pending.len() < end + 4 + size {
            return Ok(None);
        }
        let request = Request {
            method: first[0].to_owned(),
            target: first[1].to_owned(),
            protocol: first[2].to_owned(),
            headers,
            body: self.pending[end + 4..end + 4 + size].to_vec(),
        };
        self.pending.drain(..end + 4 + size);
        Ok(Some(request))
    }
}

pub(crate) struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn body(mut self, mime: &'static str, body: Vec<u8>) -> Self {
        self.headers.push(("Content-Type", mime.into()));
        self.body = body;
        self
    }

    pub fn encode(&self, request: &Request) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            415 => "Unsupported Media Type",
            454 => "Session Not Found",
            455 => "Method Not Valid in This State",
            461 => "Unsupported Transport",
            470 => "Connection Authorization Required",
            501 => "Not Implemented",
            _ => "Internal Server Error",
        };
        let mut output = format!(
            "{} {} {}\r\nServer: oc-airplay/0.1\r\nContent-Length: {}\r\n",
            request.protocol,
            self.status,
            reason,
            self.body.len()
        );
        if let Some(cseq) = request.headers.get("cseq") {
            output.push_str(&format!("CSeq: {cseq}\r\n"));
        }
        for (key, value) in &self.headers {
            output.push_str(&format!("{key}: {value}\r\n"));
        }
        output.push_str("\r\n");
        let mut bytes = output.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eof_rejects_partial_headers_and_bodies() {
        for wire in [
            b"GET /info RTSP/1.0\r\n".as_slice(),
            b"POST /stream RTSP/1.0\r\nContent-Length: 5\r\n\r\nabc".as_slice(),
        ] {
            let mut framer = Framer::default();
            framer.push(wire).unwrap();
            assert!(framer.next().unwrap().is_none());
            assert!(framer.finish().is_err());
        }
        let mut framer = Framer::default();
        framer.push(b"OPTIONS * RTSP/1.0\r\n\r\n").unwrap();
        assert!(framer.next().unwrap().is_some());
        framer.finish().unwrap();
    }

    #[test]
    fn every_fragment_boundary_preserves_binary_body_and_next_request() {
        let wire = b"POST /pair-verify RTSP/1.0\r\nCSeq: 42\r\nCoNtEnT-LeNgTh: 5\r\n\r\n\0\xff\r\nXOPTIONS * RTSP/1.0\r\nCSeq: 43\r\n\r\n";
        for split in 0..wire.len() {
            let mut framer = Framer::default();
            framer.push(&wire[..split]).unwrap();
            let first = framer.next().unwrap();
            framer.push(&wire[split..]).unwrap();
            let first = first.or_else(|| framer.next().unwrap()).unwrap();
            assert_eq!(first.body, b"\0\xff\r\nX");
            assert_eq!(first.headers["cseq"], "42");
            assert_eq!(framer.next().unwrap().unwrap().method, "OPTIONS");
            assert!(framer.next().unwrap().is_none());
        }
    }

    #[test]
    fn malformed_and_ambiguous_framing_is_rejected() {
        for wire in [
            "POST / RTSP/1.0\r\nContent-Length: 1\r\ncontent-length: 2\r\n\r\n",
            "POST / RTSP/1.0\r\nContent-Length: -1\r\n\r\n",
            "POST / RTSP/1.0\r\nContent-Length: 1048577\r\n\r\n",
            "POST / RTSP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / RTSP/1.0\r\nCSeq: 1\nInjected: yes\r\n\r\n",
            "POST / RTSP/1.0\r\n Folded: header\r\n\r\n",
            "POST / HTTP/2.0\r\n\r\n",
        ] {
            let mut framer = Framer::default();
            framer.push(wire.as_bytes()).unwrap();
            assert!(framer.next().is_err(), "{wire:?}");
        }
    }

    #[test]
    fn absolute_uri_and_response_use_original_protocol_and_cseq() {
        let mut framer = Framer::default();
        framer
            .push(b"GET rtsp://[::1]:7000/info?x=1 RTSP/1.0\r\nCSeq: 9\r\n\r\n")
            .unwrap();
        let request = framer.next().unwrap().unwrap();
        assert_eq!(request.path(), "/info");
        let response = Response::new(200)
            .body("application/octet-stream", vec![0, 255])
            .encode(&request);
        assert!(response.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert!(response.windows(9).any(|part| part == b"CSeq: 9\r\n"));
        assert!(response.ends_with(b"\r\n\r\n\0\xff"));
    }

    #[test]
    fn unterminated_headers_are_bounded() {
        let mut framer = Framer::default();
        framer.push(&vec![b'X'; MAX_HEADERS + 1]).unwrap();
        assert!(framer.next().is_err());
    }
}
