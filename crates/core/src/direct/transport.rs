//! HTTP and WebSocket transports for the direct backend.
//!
//! `http://` and `ws://` use the standard library, which is what the in-crate mock
//! speaks. `https://` uses `ureq` when the `http` feature is on. `wss://` is refused:
//! this build does not bundle a TLS WebSocket client.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use sha1::{Digest, Sha1};

use crate::error::{Error, ErrorKind, Result};

use super::protocol::path_only;

const MAX_BODY: usize = 8 * 1024 * 1024;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub trait HttpExchange: Send + Sync {
    fn exchange(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

pub trait WsConn: Send {
    fn send(&mut self, opcode: u8, data: &[u8]) -> Result<()>;
    fn recv(&mut self, timeout: Duration) -> Result<(u8, Vec<u8>)>;
}

pub trait WsDial: Send + Sync {
    fn open(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
    ) -> Result<Box<dyn WsConn>>;
}

/// `http://` via TCP. `https://` via ureq when compiled in.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoHttp;

impl HttpExchange for AutoHttp {
    fn exchange(&self, request: &HttpRequest) -> Result<HttpResponse> {
        if request.url.starts_with("https://") {
            https_exchange(request)
        } else if request.url.starts_with("http://") {
            http_exchange(request)
        } else {
            Err(Error::new(ErrorKind::InvalidInput)
                .with("field", "url")
                .detail("direct backend only calls http or https"))
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StdWs;

impl WsDial for StdWs {
    fn open(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
    ) -> Result<Box<dyn WsConn>> {
        if url.starts_with("wss://") {
            return Err(Error::new(ErrorKind::Unsupported)
                .detail("direct backend has no TLS websocket in this build"));
        }
        if !url.starts_with("ws://") {
            return Err(Error::new(ErrorKind::InvalidInput)
                .with("field", "url")
                .detail("websocket url must be ws://"));
        }
        let http_url = format!("http://{}", &url["ws://".len()..]);
        let parts = parse_http_url(&http_url)?;
        let mut stream = connect_tcp(&parts.host, parts.port, timeout)?;
        let key = websocket_key();
        let mut head = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n",
            parts.path_and_query,
            host_header(&parts)
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        let (status, resp_headers, body) = read_upgrade(&mut stream)?;
        if status != 101 {
            let failure = super::protocol::classify(status, &body);
            return Err(failure.into_error().with("url", path_only(url)));
        }
        let accept = resp_headers.iter().find_map(|(name, value)| {
            name.eq_ignore_ascii_case("sec-websocket-accept")
                .then_some(value.as_str())
        });
        let expected = accept_key(&key);
        if accept != Some(expected.as_str()) {
            return Err(Error::new(ErrorKind::Parse).detail("websocket accept mismatch"));
        }
        Ok(Box::new(TcpWs {
            stream,
            pending: body,
            pending_pos: 0,
        }))
    }
}

struct TcpWs {
    stream: TcpStream,
    pending: Vec<u8>,
    pending_pos: usize,
}

impl Read for TcpWs {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending_pos < self.pending.len() {
            let n = (self.pending.len() - self.pending_pos).min(buf.len());
            buf[..n].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
            self.pending_pos += n;
            if self.pending_pos == self.pending.len() {
                self.pending.clear();
                self.pending_pos = 0;
            }
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

impl WsConn for TcpWs {
    fn send(&mut self, opcode: u8, data: &[u8]) -> Result<()> {
        write_frame(&mut self.stream, opcode, data, true)
    }

    fn recv(&mut self, _timeout: Duration) -> Result<(u8, Vec<u8>)> {
        // Non-blocking so a caller that also writes this socket can run between polls.
        // A blocking read here holds that caller's lock and the peer never sees the bytes.
        if self.pending_pos > 0 {
            self.pending.drain(..self.pending_pos);
            self.pending_pos = 0;
        }
        self.stream.set_nonblocking(true)?;
        loop {
            if let Some((used, opcode, data)) = decode_frame(&self.pending)? {
                self.pending.drain(..used);
                return Ok((opcode, data));
            }
            let mut tmp = [0u8; 8192];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err(Error::new(ErrorKind::Io).detail("websocket closed")),
                Ok(n) => self.pending.extend_from_slice(&tmp[..n]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Err(Error::from(error));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// One websocket frame from `buf`, or `None` when more bytes are needed.
fn decode_frame(buf: &[u8]) -> Result<Option<(usize, u8, Vec<u8>)>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut length = (buf[1] & 0x7f) as usize;
    let mut offset = 2;
    if length == 126 {
        if buf.len() < offset + 2 {
            return Ok(None);
        }
        length = u16::from_be_bytes([buf[offset], buf[offset + 1]]) as usize;
        offset += 2;
    } else if length == 127 {
        if buf.len() < offset + 8 {
            return Ok(None);
        }
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[offset..offset + 8]);
        length = u64::from_be_bytes(raw) as usize;
        offset += 8;
    }
    if length > MAX_BODY {
        return Err(Error::new(ErrorKind::Parse).detail("websocket frame too large"));
    }
    let mask_len = if masked { 4 } else { 0 };
    if buf.len() < offset + mask_len + length {
        return Ok(None);
    }
    let mut data = buf[offset + mask_len..offset + mask_len + length].to_vec();
    if masked {
        let mask = &buf[offset..offset + 4];
        for (index, byte) in data.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(Some((offset + mask_len + length, opcode, data)))
}

#[cfg(feature = "http")]
fn https_exchange(request: &HttpRequest) -> Result<HttpResponse> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(request.timeout))
        .timeout_global(Some(request.timeout))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut response = https_call(&agent, request).map_err(|error| {
        Error::new(ErrorKind::Http)
            .with("url", path_only(&request.url))
            .detail(error.to_string())
    })?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY as u64)
        .read_to_vec()
        .map_err(|error| {
            Error::new(ErrorKind::Http)
                .with("url", path_only(&request.url))
                .detail(error.to_string())
        })?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(feature = "http")]
fn https_call(
    agent: &ureq::Agent,
    request: &HttpRequest,
) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    match request.method.as_str() {
        "POST" | "PUT" | "PATCH" => {
            let mut builder = match request.method.as_str() {
                "PUT" => agent.put(&request.url),
                "PATCH" => agent.patch(&request.url),
                _ => agent.post(&request.url),
            };
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if request.body.is_empty() {
                builder.send_empty()
            } else {
                builder.send(request.body.as_slice())
            }
        }
        _ => {
            let mut builder = if request.method == "DELETE" {
                agent.delete(&request.url)
            } else {
                agent.get(&request.url)
            };
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if request.body.is_empty() {
                builder.call()
            } else {
                builder.force_send_body().send(request.body.as_slice())
            }
        }
    }
}

#[cfg(not(feature = "http"))]
fn https_exchange(request: &HttpRequest) -> Result<HttpResponse> {
    let _ = request;
    Err(Error::new(ErrorKind::HttpUnavailable).detail("https support was compiled out"))
}

fn http_exchange(request: &HttpRequest) -> Result<HttpResponse> {
    let parts = parse_http_url(&request.url)?;
    let mut stream = connect_tcp(&parts.host, parts.port, request.timeout)?;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        request.method,
        parts.path_and_query,
        host_header(&parts),
        request.body.len()
    );
    for (name, value) in &request.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !request.body.is_empty() {
        stream.write_all(&request.body)?;
    }
    let (status, headers, body) = read_http(&mut stream, false)?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

struct UrlParts {
    host: String,
    port: u16,
    path_and_query: String,
    default_port: bool,
}

fn parse_http_url(url: &str) -> Result<UrlParts> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput)
            .with("field", "url")
            .detail("missing scheme")
    })?;
    let tls = match scheme {
        "http" | "ws" => false,
        "https" | "wss" => true,
        _ => {
            return Err(Error::new(ErrorKind::InvalidInput)
                .with("field", "url")
                .detail("unsupported scheme"));
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(Error::invalid("url", "missing host"));
    }
    let (host, port, default_port) = if let Some((host, port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && !port.is_empty()
        && !host.contains(':')
    {
        let port = port
            .parse::<u16>()
            .map_err(|_| Error::invalid("url", "bad port"))?;
        (host.to_owned(), port, false)
    } else {
        (authority.to_owned(), if tls { 443 } else { 80 }, true)
    };
    let path_and_query = if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    };
    Ok(UrlParts {
        host,
        port,
        path_and_query,
        default_port,
    })
}

fn host_header(parts: &UrlParts) -> String {
    if parts.default_port {
        parts.host.clone()
    } else {
        format!("{}:{}", parts.host, parts.port)
    }
}

fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let mut last = None;
    let addrs = (host, port).to_socket_addrs().map_err(|error| {
        Error::new(ErrorKind::Http)
            .with("url", host)
            .detail(error.to_string())
    })?;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last
        .map(Error::from)
        .unwrap_or_else(|| Error::new(ErrorKind::Http).detail("no address")))
}

/// Headers, then the body only when the status is not `101`. A successful upgrade
/// leaves the following bytes for websocket frames.
fn read_upgrade(stream: &mut TcpStream) -> Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let (status, headers, leftover) = read_headers(stream)?;
    if status == 101 {
        return Ok((status, headers, leftover));
    }
    let length = headers.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.parse::<usize>().ok())
            .flatten()
    });
    let mut body = leftover;
    if let Some(length) = length {
        let mut tmp = [0u8; 1024];
        while body.len() < length && body.len() < MAX_BODY {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(length.min(MAX_BODY));
    }
    Ok((status, headers, body))
}

fn read_headers(stream: &mut TcpStream) -> Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let split_at = loop {
        if buf.len() > 64 * 1024 {
            return Err(Error::new(ErrorKind::Parse).detail("http headers too large"));
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(index) = find_header_end(&buf) {
            break Some(index);
        }
    };
    let Some(index) = split_at else {
        return Err(Error::new(ErrorKind::Parse).detail("http response ended early"));
    };
    let header_text = String::from_utf8_lossy(&buf[..index]);
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::new(ErrorKind::Parse).detail("bad http status line"))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    Ok((status, headers, buf[index + 4..].to_vec()))
}

fn read_http(
    stream: &mut TcpStream,
    headers_only: bool,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let split_at = loop {
        if buf.len() > 64 * 1024 {
            return Err(Error::new(ErrorKind::Parse).detail("http headers too large"));
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(index) = find_header_end(&buf) {
            break Some(index);
        }
    };
    let Some(index) = split_at else {
        return Err(Error::new(ErrorKind::Parse).detail("http response ended early"));
    };
    let header_bytes = buf[..index].to_vec();
    let mut rest = buf[index + 4..].to_vec();
    let header_text = String::from_utf8_lossy(&header_bytes);
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::new(ErrorKind::Parse).detail("bad http status line"))?;
    let mut headers = Vec::new();
    let mut length = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_owned();
            let value = value.trim().to_owned();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().ok();
            }
            headers.push((name, value));
        }
    }
    if headers_only {
        return Ok((status, headers, Vec::new()));
    }
    if let Some(length) = length {
        if length > MAX_BODY {
            return Err(Error::new(ErrorKind::Parse).detail("http body too large"));
        }
        while rest.len() < length {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&tmp[..n]);
        }
        rest.truncate(length);
        return Ok((status, headers, rest));
    }
    loop {
        if rest.len() > MAX_BODY {
            return Err(Error::new(ErrorKind::Parse).detail("http body too large"));
        }
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => break,
            Err(error) => return Err(error.into()),
        };
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&tmp[..n]);
    }
    Ok((status, headers, rest))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

pub fn websocket_key() -> String {
    let mixed = format!("{}{}", crate::accounts::nonce(), crate::accounts::nonce());
    let bytes = mixed.as_bytes();
    let mut raw = [0u8; 16];
    let n = bytes.len().min(16);
    raw[..n].copy_from_slice(&bytes[..n]);
    B64.encode(raw)
}

pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    B64.encode(hasher.finalize())
}

pub fn write_frame(stream: &mut impl Write, opcode: u8, data: &[u8], mask: bool) -> Result<()> {
    if data.len() > MAX_BODY {
        return Err(Error::new(ErrorKind::InvalidInput).detail("websocket frame too large"));
    }
    let mut header = Vec::with_capacity(14 + data.len());
    header.push(0x80 | (opcode & 0x0f));
    let mask_bit = if mask { 0x80 } else { 0 };
    if data.len() < 126 {
        header.push(mask_bit | data.len() as u8);
    } else if data.len() <= u16::MAX as usize {
        header.push(mask_bit | 126);
        header.extend_from_slice(&(data.len() as u16).to_be_bytes());
    } else {
        header.push(mask_bit | 127);
        header.extend_from_slice(&(data.len() as u64).to_be_bytes());
    }
    if mask {
        let key = mask_key();
        header.extend_from_slice(&key);
        stream.write_all(&header)?;
        let mut masked = data.to_vec();
        for (index, byte) in masked.iter_mut().enumerate() {
            *byte ^= key[index % 4];
        }
        stream.write_all(&masked)?;
    } else {
        stream.write_all(&header)?;
        stream.write_all(data)?;
    }
    stream.flush()?;
    Ok(())
}

fn mask_key() -> [u8; 4] {
    let nonce = crate::accounts::nonce();
    let bytes = nonce.as_bytes();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

#[cfg(test)]
pub fn read_frame(stream: &mut impl Read) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut length = (head[1] & 0x7f) as u64;
    if length == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext)?;
        length = u64::from(u16::from_be_bytes(ext));
    } else if length == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext)?;
        length = u64::from_be_bytes(ext);
    }
    if length > MAX_BODY as u64 {
        return Err(Error::new(ErrorKind::Parse).detail("websocket frame too large"));
    }
    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask)?;
    }
    let mut data = vec![0u8; length as usize];
    if length > 0 {
        stream.read_exact(&mut data)?;
    }
    if masked {
        for (index, byte) in data.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((opcode, data))
}
