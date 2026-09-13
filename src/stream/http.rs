//! A small HTTP/1.1 client, enough to talk to an object store.
//!
//! Not a general-purpose client: no redirects, no keep-alive pooling, no
//! compression negotiation. It does exactly what S3-compatible storage needs —
//! PUT, GET, DELETE, and a GET with a query string for listing — and reads
//! both `Content-Length` and `Transfer-Encoding: chunked` responses, because
//! MinIO uses the latter for some list responses.
//!
//! **Plaintext only.** TLS would mean a dependency, and a hand-rolled TLS
//! stack would be worse than a dependency. For an in-cluster MinIO or Ceph
//! endpoint on a private network that is the normal deployment anyway; for
//! anything reached over the public internet, use the `exec` backend and let
//! `aws`, `mc` or `rclone` handle the transport.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub port: u16,
    /// Path plus optional query string, already percent-encoded.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub fn send(req: &Request) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect((req.host, req.port))?;
    stream.set_read_timeout(Some(req.timeout))?;
    stream.set_write_timeout(Some(req.timeout))?;

    // The Host header must match the one that was signed, byte for byte —
    // including the port for non-default ports. Getting this wrong produces a
    // SignatureDoesNotMatch that looks like a credentials problem.
    let host_header = if req.port == 80 || req.port == 443 {
        req.host.to_string()
    } else {
        format!("{}:{}", req.host, req.port)
    };
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.target);
    head.push_str(&format!("Host: {host_header}\r\n"));
    head.push_str("Connection: close\r\n");
    head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    for (k, v) in &req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    if !req.body.is_empty() {
        stream.write_all(&req.body)?;
    }
    stream.flush()?;

    let mut reader = BufReader::new(stream);

    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line: {status_line:?}")))?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let find = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.to_lowercase())
    };

    let body = if find("transfer-encoding").as_deref() == Some("chunked") {
        read_chunked(&mut reader)?
    } else if let Some(len) = find("content-length").and_then(|v| v.parse::<usize>().ok()) {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        buf
    } else {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        buf
    };

    Ok(Response {
        status,
        headers,
        body,
    })
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| std::io::Error::other(format!("bad chunk size: {size_str:?}")))?;
        if size == 0 {
            // Trailers, then a final blank line.
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(out)
}

/// RFC 3986 unreserved set, which is what SigV4 wants for path and query
/// encoding. Note `/` is *not* encoded in paths but *is* in query values.
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
