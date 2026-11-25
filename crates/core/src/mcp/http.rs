//! Just enough HTTP/1.1 for one endpoint on this machine.
//!
//! The listener serves one path of its own on loopback, so this is a reader and a writer rather
//! than a server: no chunked bodies, no compression, no pipelining beyond what a client that waits
//! for each reply does, and a cap on both the header block and the body so a request cannot make
//! the app allocate without end. Anything the endpoint does not understand is answered with a
//! status the client can read, never with a panic.

use std::io::{BufRead, ErrorKind as IoErrorKind, Write};

use crate::error::{Error, ErrorKind, Result};

/// How much of a request's head is read before it is refused.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// How large a request body may be. A tool call is a few hundred bytes; the limit is for the
/// process's sake, not the protocol's.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// One parsed request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub method: String,
    /// The request target as it arrived (`/mcp`), query string included.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A header's value, matched without regard to case, which is how HTTP names them.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The path of the request target, without its query string.
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(path, _)| path)
    }

    /// The token in `Authorization: Bearer …`, if the request carries one.
    pub fn bearer(&self) -> Option<&str> {
        let value = self.header("authorization")?;
        let (scheme, token) = value.split_once(' ')?;
        scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| token.trim())
            .filter(|token| !token.is_empty())
    }
}

/// What reading one request found.
#[derive(Debug, PartialEq)]
pub enum ReadOutcome {
    Request(Request),
    /// The peer closed the connection, which is how a `Connection: close` ends.
    Closed,
}

fn invalid(what: impl Into<String>) -> Error {
    Error::invalid("request", what)
}

/// Reads one request from a connection.
///
/// A body is read only when `Content-Length` says so, and a request that promises more than
/// [`MAX_BODY_BYTES`], or one that would need a decoder this endpoint does not carry, is refused
/// with an error the caller turns into a status.
pub fn read_request(reader: &mut impl BufRead) -> Result<ReadOutcome> {
    let mut line = String::new();
    // A client that connects and sends nothing, or closes, is not an error.
    let read = reader.read_line(&mut line).map_err(|e| {
        Error::new(ErrorKind::Http).detail(format!("reading the request line: {e}"))
    })?;
    if read == 0 {
        return Ok(ReadOutcome::Closed);
    }
    let mut parts = line.trim_end().split(' ');
    let (method, target) = match (parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version)) if version.starts_with("HTTP/1.") => {
            (method.to_owned(), target.to_owned())
        }
        _ => {
            return Err(invalid(format!(
                "a request line the endpoint cannot read: {line:?}"
            )));
        }
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut used = line.len();
    loop {
        let mut header = String::new();
        let read = reader.read_line(&mut header).map_err(|e| {
            Error::new(ErrorKind::Http).detail(format!("reading a header line: {e}"))
        })?;
        if read == 0 {
            return Err(invalid("the connection ended inside the headers"));
        }
        used += header.len();
        if used > MAX_HEAD_BYTES {
            return Err(invalid("the headers are longer than the endpoint reads"));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| invalid(format!("a header line with no colon: {header:?}")))?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }

    let request = Request {
        method,
        target,
        headers,
        body: Vec::new(),
    };
    // A body comes only with a length. Anything that needs a transfer encoding is refused: a
    // local MCP client sends one JSON object, so there is nothing to gain by decoding.
    if request.header("transfer-encoding").is_some() {
        return Err(Error::new(ErrorKind::Unsupported).detail("a chunked request body"));
    }
    let length = match request.header("content-length") {
        Some(value) => value
            .trim()
            .parse::<usize>()
            .map_err(|_| invalid(format!("a content-length that is not a number: {value:?}")))?,
        None => 0,
    };
    if length > MAX_BODY_BYTES {
        return Err(invalid(format!(
            "a body of {length} bytes, over the endpoint's limit of {MAX_BODY_BYTES}"
        )));
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|e| invalid(format!("the body ended early: {e}")))?;
    }
    Ok(ReadOutcome::Request(Request { body, ..request }))
}

/// The statuses this endpoint answers with, and the phrase that goes with each.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// A response to write back.
#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Headers this endpoint adds, such as the session id.
    pub extra: Vec<(&'static str, String)>,
}

impl Response {
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            content_type: "application/json",
            body: body.into(),
            extra: Vec::new(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
            extra: Vec::new(),
        }
    }

    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.extra.push((name, value.into()));
        self
    }

    /// The bytes of the response: a status line, the headers, and the body as it is.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status)).into_bytes();
        out.extend_from_slice(format!("Content-Type: {}\r\n", self.content_type).as_bytes());
        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        out.extend_from_slice(b"Cache-Control: no-store\r\n");
        for (name, value) in &self.extra {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }

    pub fn write_to(&self, writer: &mut impl Write) -> std::io::Result<()> {
        writer.write_all(&self.bytes())?;
        writer.flush()
    }
}

/// A "client hung up" test for callers that read a socket directly.
pub fn is_disconnect(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        IoErrorKind::UnexpectedEof
            | IoErrorKind::ConnectionReset
            | IoErrorKind::ConnectionAborted
            | IoErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn reader(bytes: &[u8]) -> BufReader<Cursor<Vec<u8>>> {
        BufReader::new(Cursor::new(bytes.to_vec()))
    }

    fn post(body: &str, headers: &str) -> Vec<u8> {
        format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn a_request_is_read_with_its_body_and_headers() {
        let bytes = post(r#"{"jsonrpc":"2.0"}"#, "Authorization: Bearer abc\r\n");
        let ReadOutcome::Request(request) = read_request(&mut reader(&bytes)).expect("read") else {
            panic!("a request");
        };
        assert_eq!(request.method, "POST");
        assert_eq!(request.path(), "/mcp");
        assert_eq!(request.bearer(), Some("abc"));
        assert_eq!(request.body, br#"{"jsonrpc":"2.0"}"#);
    }

    #[test]
    fn header_names_are_matched_without_case() {
        let bytes = post("x", "AUTHORIZATION: bearer tOkEn\r\n");
        let ReadOutcome::Request(request) = read_request(&mut reader(&bytes)).expect("read") else {
            panic!("a request");
        };
        assert_eq!(request.bearer(), Some("tOkEn"));
    }

    #[test]
    fn a_query_string_is_not_part_of_the_path() {
        let bytes = b"GET /mcp?x=1 HTTP/1.1\r\nHost: h\r\n\r\n";
        let ReadOutcome::Request(request) = read_request(&mut reader(bytes)).expect("read") else {
            panic!("a request");
        };
        assert_eq!(request.path(), "/mcp");
        assert_eq!(request.target, "/mcp?x=1");
    }

    #[test]
    fn a_closed_connection_is_not_an_error() {
        assert_eq!(
            read_request(&mut reader(b"")).expect("clean"),
            ReadOutcome::Closed
        );
    }

    #[test]
    fn the_endpoint_refuses_what_it_cannot_read() {
        // Four requests the endpoint will not read, and one it cannot decode: the kind says which.
        let cases: [(&[u8], ErrorKind); 5] = [
            (b"nonsense\r\n\r\n", ErrorKind::InvalidInput),
            (b"POST /mcp HTTP/2.0\r\n\r\n", ErrorKind::InvalidInput),
            (
                b"POST /mcp HTTP/1.1\r\nHost h\r\n\r\n",
                ErrorKind::InvalidInput,
            ),
            (
                b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
                ErrorKind::Unsupported,
            ),
            (
                b"POST /mcp HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n",
                ErrorKind::InvalidInput,
            ),
        ];
        for (case, kind) in cases {
            let error = read_request(&mut reader(case)).expect_err("refused");
            assert_eq!(error.kind, kind, "{case:?}");
        }
    }

    #[test]
    fn a_head_that_never_ends_is_refused_not_allocated_for() {
        let mut bytes = b"POST /mcp HTTP/1.1\r\n".to_vec();
        for n in 0..MAX_HEAD_BYTES {
            bytes.extend_from_slice(format!("X-Pad-{n}: v\r\n").as_bytes());
        }
        let error = read_request(&mut reader(&bytes)).expect_err("refused");
        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn a_response_carries_its_length_and_extra_headers() {
        let response = Response::json(200, br#"{"ok":true}"#.to_vec()).with("Mcp-Session-Id", "s1");
        let text = String::from_utf8(response.bytes()).expect("text");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.contains("Mcp-Session-Id: s1\r\n"));
        assert!(text.ends_with(r#"{"ok":true}"#));
    }

    #[test]
    fn a_response_can_be_written_to_a_connection() {
        let mut out: Vec<u8> = Vec::new();
        Response::text(401, "no token")
            .write_to(&mut out)
            .expect("written");
        let text = String::from_utf8(out).expect("text");
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(text.ends_with("no token"));
    }

    #[test]
    fn a_peer_that_hung_up_is_recognized() {
        assert!(is_disconnect(&std::io::Error::from(
            IoErrorKind::BrokenPipe
        )));
        assert!(!is_disconnect(&std::io::Error::from(
            IoErrorKind::InvalidData
        )));
    }
}
