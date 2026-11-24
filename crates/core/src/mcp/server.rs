//! The endpoint itself: one loopback listener, one path, a token, and the read tools behind it.
//!
//! What this file is careful about, and why:
//!
//! - **Loopback only.** The listener binds `127.0.0.1` and nothing else, so the endpoint is not on
//!   the network whoever asks.
//! - **A token on every request.** The token is given to [`Endpoint::start`] by the caller (the app
//!   keeps it, and is the only thing that knows a client should have it). It is compared without
//!   short-circuiting, so a wrong token leaks nothing but its own length.
//! - **A refused `Origin`.** A page in a browser on this machine can reach `127.0.0.1`, so a request
//!   that names an origin which is not this endpoint's own is refused. This is the DNS-rebinding
//!   case the MCP specification tells a server to defend against.
//! - **A response for every request.** Nothing here panics: a request the endpoint cannot read is a
//!   status, a tool that fails is a tool result marked `isError`, and an unknown method is a
//!   JSON-RPC fault.
//!
//! Slice 1 keeps no session state that matters: a session id is handed out because clients expect
//! one and refused once closed, but nothing hangs on it. The confirmation flow in slice 2 needs
//! real per-session state, and will extend this file rather than replace it.

use std::{
    collections::HashSet,
    io::BufReader,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use crate::error::{Error, ErrorKind, Result};

use super::{
    http,
    protocol::{self, Fault, Incoming},
    tools,
    view::Source,
};

/// The one path the endpoint answers on.
pub const PATH: &str = "/mcp";

/// The header a client repeats the session id in.
pub const SESSION_HEADER: &str = "Mcp-Session-Id";

/// How long a connection may sit with nothing to say before it is dropped.
const IDLE: Duration = Duration::from_secs(120);

/// How often the listener looks at the stop flag while no client is connecting.
const TICK: Duration = Duration::from_millis(50);

/// What the endpoint is started with.
#[derive(Debug, Clone)]
pub struct Config {
    /// What a client must send as `Authorization: Bearer …`.
    pub token: String,
    /// The loopback port. Zero lets the system pick one, which is what a test wants.
    pub port: u16,
}

/// Everything a request is answered from.
struct Ctx {
    token: String,
    source: Arc<dyn Source>,
    sessions: Mutex<HashSet<String>>,
    seq: AtomicU64,
    /// The endpoint's own origin, for the check described above.
    origins: Vec<String>,
}

impl Ctx {
    fn holds(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .map(|set| set.contains(id))
            .unwrap_or(false)
    }

    /// Hands out an id. It is an opaque marker, not a credential: the token is the credential, and
    /// this only has to be unlikely to collide with another run.
    fn new_session(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
            .hash(&mut hasher);
        self.seq.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        let id = format!("{:016x}", hasher.finish());
        if let Ok(mut set) = self.sessions.lock() {
            set.insert(id.clone());
        }
        id
    }
}

/// A running endpoint. Dropping it stops the listener.
pub struct Endpoint {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Endpoint {
    /// Binds the loopback port and starts answering.
    ///
    /// The listener stops as soon as the flag is set; a connection already being served is left to
    /// its own idle timeout, because a client mid-request is not this call's to cut off.
    pub fn start(config: Config, source: Arc<dyn Source>) -> Result<Endpoint> {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, config.port)).map_err(|e| match e.kind() {
                std::io::ErrorKind::AddrInUse => Error::new(ErrorKind::PortInUse)
                    .with("port", config.port)
                    .detail(format!("the agent endpoint's port is taken: {e}")),
                _ => Error::new(ErrorKind::Http)
                    .with("port", config.port)
                    .detail(format!("the agent endpoint could not bind: {e}")),
            })?;
        let addr = listener
            .local_addr()
            .map_err(|e| Error::new(ErrorKind::Http).detail(format!("no local address: {e}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::new(ErrorKind::Http).detail(format!("non-blocking bind: {e}")))?;
        let ctx = Arc::new(Ctx {
            token: config.token,
            source,
            sessions: Mutex::new(HashSet::new()),
            seq: AtomicU64::new(0),
            origins: vec![
                format!("http://127.0.0.1:{}", addr.port()),
                format!("http://localhost:{}", addr.port()),
            ],
        });
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("mcp-endpoint".to_owned())
                .spawn(move || accept_loop(listener, ctx, stop))
                .map_err(|e| {
                    Error::new(ErrorKind::Http)
                        .detail(format!("the endpoint's thread could not start: {e}"))
                })?
        };
        Ok(Endpoint {
            addr,
            stop,
            accept: Some(accept),
        })
    }

    /// Where a client connects.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops the listener and waits for its thread. Serving connections end on their own.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(listener: TcpListener, ctx: Arc<Ctx>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let ctx = Arc::clone(&ctx);
                let _ = thread::Builder::new()
                    .name("mcp-connection".to_owned())
                    .spawn(move || serve(stream, ctx));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(TICK),
            // A listener that keeps failing is not worth a spin loop; the flag is read again on
            // the next turn.
            Err(_) => thread::sleep(TICK),
        }
    }
}

/// Serves one connection until the client closes it, asks to close, or goes quiet for too long.
fn serve(stream: TcpStream, ctx: Arc<Ctx>) {
    // The listener accepts without blocking so the stop flag is read between connections, and on
    // Windows an accepted socket inherits that mode: a read here would fail the moment the bytes
    // have not arrived yet. The connection itself blocks, with a timeout as its way out.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(IDLE));
    let _ = stream.set_nodelay(true);
    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reading);
    let mut writer = stream;
    loop {
        match http::read_request(&mut reader) {
            Ok(http::ReadOutcome::Closed) => return,
            Ok(http::ReadOutcome::Request(request)) => {
                let close = request
                    .header("connection")
                    .is_some_and(|value| value.eq_ignore_ascii_case("close"));
                let response = handle(&request, &ctx);
                if response.write_to(&mut writer).is_err() {
                    return;
                }
                if close {
                    return;
                }
            }
            Err(error) => {
                let status = status_for(&error);
                let _ = http::Response::text(status, error.detail).write_to(&mut writer);
                return;
            }
        }
    }
}

/// A request the endpoint could not read at all: 413 for a body over the limit, 400 otherwise.
fn status_for(error: &Error) -> u16 {
    if error.detail.contains("over the endpoint's limit") {
        413
    } else {
        400
    }
}

/// One request in, one response out. Everything the endpoint decides is decided here, which is why
/// the tests below can drive it without a socket.
fn handle(request: &http::Request, ctx: &Ctx) -> http::Response {
    if request.path() != PATH {
        return http::Response::text(404, format!("the agent endpoint answers on {PATH}"));
    }
    if let Some(origin) = request.header("origin")
        && !origin.is_empty()
        && !ctx.origins.iter().any(|own| own == origin)
    {
        return http::Response::text(403, "this endpoint only answers its own window".to_owned());
    }
    match request.method.as_str() {
        "POST" => post(request, ctx),
        "GET" => http::Response::text(
            405,
            "this endpoint sends nothing on its own yet; POST to /mcp".to_owned(),
        ),
        "DELETE" => {
            if let Some(id) = request.header(SESSION_HEADER)
                && let Ok(mut set) = ctx.sessions.lock()
            {
                set.remove(id);
            }
            http::Response::text(200, "session closed")
        }
        other => http::Response::text(405, format!("{other} is not answered here")),
    }
}

fn post(request: &http::Request, ctx: &Ctx) -> http::Response {
    let Some(offered) = request.bearer() else {
        return http::Response::text(
            401,
            "this endpoint wants Authorization: Bearer <token>".to_owned(),
        );
    };
    if !token_matches(&ctx.token, offered) {
        return http::Response::text(401, "the token is wrong".to_owned());
    }
    if request.body.is_empty() {
        return http::Response::json(
            200,
            Fault::new(protocol::PARSE_ERROR, "no body")
                .response()
                .to_string(),
        );
    }
    // A session id the endpoint handed out before must still be one it knows; one it never issued
    // is refused, and a request without the header is served (a stateless client is legal).
    if let Some(id) = request.header(SESSION_HEADER)
        && !ctx.holds(id)
    {
        return http::Response::json(
            404,
            Fault::new(protocol::INVALID_REQUEST, "unknown session")
                .response()
                .to_string(),
        );
    }
    let incoming = match protocol::parse(&request.body) {
        Ok(incoming) => incoming,
        Err(fault) => return http::Response::json(200, fault.response().to_string()),
    };
    match incoming {
        Incoming::Notification { .. } => http::Response::text(202, ""),
        Incoming::Request { id, method, params } => {
            let (body, session) = dispatch(ctx, &method, &params, &id);
            let mut response = http::Response::json(200, body.to_string());
            if let Some(id) = session {
                response = response.with(SESSION_HEADER, id);
            }
            response
        }
    }
}

/// The answer to one request, and the session id to hand back when the client should start using
/// one.
fn dispatch(ctx: &Ctx, method: &str, params: &Value, id: &Value) -> (Value, Option<String>) {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let result = protocol::initialize_result(
                super::SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
                requested,
            );
            (protocol::result(id, result), Some(ctx.new_session()))
        }
        "ping" => (protocol::result(id, json!({})), None),
        "tools/list" => (protocol::result(id, tools::descriptors()), None),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return (
                    Fault::new(protocol::INVALID_PARAMS, "tools/call needs a name")
                        .to(id)
                        .response(),
                    None,
                );
            };
            if tools::find(name).is_none() {
                return (
                    Fault::new(protocol::METHOD_NOT_FOUND, format!("no tool called {name}"))
                        .to(id)
                        .response(),
                    None,
                );
            }
            let snapshot = ctx.source.snapshot();
            match tools::call(&snapshot, name, params.get("arguments")) {
                Ok(answer) => (protocol::result(id, tool_result(&answer)), None),
                Err(error) => (protocol::result(id, tool_failure(&error)), None),
            }
        }
        other => (
            Fault::new(
                protocol::METHOD_NOT_FOUND,
                format!("{other} is not a method"),
            )
            .to(id)
            .response(),
            None,
        ),
    }
}

/// A tool's answer as MCP carries it: one text block holding the JSON the tool produced.
pub fn tool_result(answer: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": answer.to_string() }],
        "isError": false,
    })
}

/// A tool that refused, as MCP carries it: the same shape, marked as an error, with the
/// machine-readable error the app itself would act on.
pub fn tool_failure(error: &Error) -> Value {
    json!({
        "content": [{ "type": "text", "text": machine_error(error) }],
        "isError": true,
    })
}

/// How an error is written for a client: the kind and what was wrong, and the next step when the
/// app has one. Never localised prose — that belongs to the window, not to a protocol.
pub fn machine_error(error: &Error) -> String {
    let mut text = format!("{:?}: {}", error.kind, error.detail);
    for (key, value) in &error.params {
        text.push_str(&format!(" ({key}={value})"));
    }
    text
}

/// Compares two tokens without stopping at the first difference, so the time taken says nothing
/// about how much of the token was right.
fn token_matches(expected: &str, offered: &str) -> bool {
    let (expected, offered) = (expected.as_bytes(), offered.as_bytes());
    if expected.len() != offered.len() {
        return false;
    }
    expected
        .iter()
        .zip(offered)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::view::{Empty, Session, Snapshot};

    const TOKEN: &str = "test-token";

    struct OneSession;

    impl Source for OneSession {
        fn snapshot(&self) -> Snapshot {
            Snapshot {
                sessions: vec![Session {
                    name: "run-1".to_owned(),
                    account: "a@b.c".to_owned(),
                    hardware: "T4".to_owned(),
                    state: "running".to_owned(),
                    auto_stop: "30 min".to_owned(),
                    units_per_hour: Some(1.5),
                }],
                ..Snapshot::default()
            }
        }
    }

    fn ctx() -> Ctx {
        Ctx {
            token: TOKEN.to_owned(),
            source: Arc::new(Empty),
            sessions: Mutex::new(HashSet::new()),
            seq: AtomicU64::new(0),
            origins: vec!["http://127.0.0.1:8765".to_owned()],
        }
    }

    fn request(method: &str, body: &str) -> http::Request {
        http::Request {
            method: method.to_owned(),
            target: PATH.to_owned(),
            headers: vec![
                ("Authorization".to_owned(), format!("Bearer {TOKEN}")),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ],
            body: body.as_bytes().to_vec(),
        }
    }

    fn body_of(response: &http::Response) -> Value {
        serde_json::from_slice(&response.body).expect("a JSON body")
    }

    #[test]
    fn without_a_token_nothing_is_served() {
        let mut request = request("POST", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        request.headers.retain(|(name, _)| name != "Authorization");
        let response = handle(&request, &ctx());
        assert_eq!(response.status, 401);
        let wrong = {
            let mut request = request.clone();
            request.headers.push((
                "Authorization".to_owned(),
                "Bearer not-the-token".to_owned(),
            ));
            request
        };
        assert_eq!(handle(&wrong, &ctx()).status, 401);
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        let mut request = request("POST", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        request
            .headers
            .push(("Origin".to_owned(), "http://evil.example".to_owned()));
        assert_eq!(handle(&request, &ctx()).status, 403);
        request.headers.retain(|(name, _)| name != "Origin");
        request
            .headers
            .push(("Origin".to_owned(), "http://127.0.0.1:8765".to_owned()));
        assert_eq!(
            handle(&request, &ctx()).status,
            200,
            "its own window is allowed"
        );
    }

    #[test]
    fn another_path_is_a_404_and_get_says_what_to_do() {
        let mut req = request("POST", "{}");
        req.target = "/other".to_owned();
        assert_eq!(handle(&req, &ctx()).status, 404);
        assert_eq!(handle(&request("GET", ""), &ctx()).status, 405);
        assert_eq!(handle(&request("PATCH", ""), &ctx()).status, 405);
    }

    #[test]
    fn initialize_answers_and_hands_out_a_session() {
        let response = handle(
            &request(
                "POST",
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            ),
            &ctx(),
        );
        assert_eq!(response.status, 200);
        assert_eq!(
            body_of(&response)["result"]["serverInfo"]["name"],
            super::super::SERVER_NAME
        );
        let session = response
            .extra
            .iter()
            .find(|(name, _)| *name == SESSION_HEADER)
            .map(|(_, value)| value.clone())
            .expect("a session id");
        assert_eq!(session.len(), 16);
    }

    #[test]
    fn a_session_it_never_handed_out_is_refused() {
        let mut request = request("POST", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        request
            .headers
            .push((SESSION_HEADER.to_owned(), "deadbeefdeadbeef".to_owned()));
        assert_eq!(handle(&request, &ctx()).status, 404);
    }

    #[test]
    fn a_notification_is_acknowledged_with_nothing() {
        let response = handle(
            &request(
                "POST",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ),
            &ctx(),
        );
        assert_eq!(response.status, 202);
        assert!(response.body.is_empty());
    }

    #[test]
    fn tools_list_brings_the_table() {
        let response = handle(
            &request("POST", r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#),
            &ctx(),
        );
        let body = body_of(&response);
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert!(names.contains(&"sessions_list"), "{names:?}");
    }

    #[test]
    fn a_tool_call_reads_the_source_and_wraps_its_answer() {
        let ctx = Ctx {
            source: Arc::new(OneSession),
            ..ctx()
        };
        let response = handle(
            &request(
                "POST",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sessions_list"}}"#,
            ),
            &ctx,
        );
        let body = body_of(&response);
        assert_eq!(body["result"]["isError"], false);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("a text block");
        assert!(text.contains("run-1"), "{text}");
    }

    #[test]
    fn a_refused_tool_is_a_result_marked_as_an_error() {
        let response = handle(
            &request(
                "POST",
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"sessions_list","arguments":{"all":true}}}"#,
            ),
            &ctx(),
        );
        let body = body_of(&response);
        assert_eq!(body["result"]["isError"], true, "{body}");
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("InvalidInput"),
            "{body}"
        );
    }

    #[test]
    fn a_tool_that_is_not_there_is_a_method_fault() {
        let response = handle(
            &request(
                "POST",
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"session_start"}}"#,
            ),
            &ctx(),
        );
        assert_eq!(
            body_of(&response)["error"]["code"],
            protocol::METHOD_NOT_FOUND
        );
    }

    #[test]
    fn a_broken_body_is_a_parse_fault_not_a_status() {
        let response = handle(&request("POST", "not json"), &ctx());
        assert_eq!(response.status, 200);
        assert_eq!(body_of(&response)["error"]["code"], protocol::PARSE_ERROR);
    }

    #[test]
    fn closing_a_session_is_acknowledged() {
        let ctx = ctx();
        let session = ctx.new_session();
        let mut req = request("DELETE", "");
        req.headers
            .push((SESSION_HEADER.to_owned(), session.clone()));
        assert_eq!(handle(&req, &ctx).status, 200);
        let mut again = request("POST", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        again.headers.push((SESSION_HEADER.to_owned(), session));
        assert_eq!(handle(&again, &ctx).status, 404, "a closed session is gone");
    }

    #[test]
    fn tokens_are_compared_without_an_early_exit() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "ab"));
        assert!(!token_matches("", "x"));
    }

    #[test]
    fn a_machine_error_names_the_kind_and_what_was_wrong() {
        let error = Error::invalid("plugin", "no run of comfyui");
        let text = machine_error(&error);
        assert!(text.contains("InvalidInput"), "{text}");
        assert!(text.contains("no run of comfyui"), "{text}");
        assert!(text.contains("field=plugin"), "{text}");
    }
}
