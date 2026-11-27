//! The agent endpoint over a real socket: a client connects, names itself, lists the tools, calls
//! one, and is refused when its token or its origin is wrong.
//!
//! This is the test that says the endpoint works rather than that its parts do: everything here
//! goes through the loopback listener the app would start, in the order a client would.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Arc,
    time::Duration,
};

use colab_playground_core::{
    Result,
    mcp::{
        Config, Endpoint, PATH, SESSION_HEADER,
        actions::{Action, Actions},
        protocol,
        view::{Session, Snapshot, Source},
    },
};

const TOKEN: &str = "a-token-only-this-test-knows";

/// An executor that answers the way the app would, and remembers what it was asked — including
/// which client asked, which is what the app's dialog shows.
#[derive(Default)]
struct Recorder {
    asked: std::sync::Mutex<Vec<(Action, String)>>,
}

impl Recorder {
    fn asked(&self) -> Vec<(Action, String)> {
        self.asked.lock().expect("not poisoned").clone()
    }
}

impl Actions for Recorder {
    fn perform(&self, action: Action, client: &str) -> Result<serde_json::Value> {
        self.asked
            .lock()
            .expect("not poisoned")
            .push((action.clone(), client.to_owned()));
        Ok(serde_json::json!({ "done": action.name() }))
    }
}

/// A source with one runtime, so a tool has something to answer with.
struct Demo;

impl Source for Demo {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            sessions: vec![Session {
                name: "playground-1".to_owned(),
                account: "someone@example.com".to_owned(),
                hardware: "T4".to_owned(),
                state: "running".to_owned(),
                auto_stop: "30 min".to_owned(),
                units_per_hour: Some(1.5),
            }],
            ..Snapshot::default()
        }
    }
}

fn endpoint() -> Endpoint {
    endpoint_with(Arc::new(Recorder::default()))
}

fn endpoint_with(actions: Arc<dyn Actions>) -> Endpoint {
    Endpoint::start(
        Config {
            token: TOKEN.to_owned(),
            // Let the system pick a free port: two runs of this test must not collide.
            port: 0,
            actions,
        },
        Arc::new(Demo),
    )
    .expect("the endpoint binds loopback")
}

/// One response, read the way a client reads it: status, headers, then the body its length promises.
struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

fn read_answer(reader: &mut BufReader<TcpStream>) -> Answer {
    let mut line = String::new();
    reader.read_line(&mut line).expect("a status line");
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {line:?}"));
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("a header");
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').expect("a header with a colon");
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .expect("a content length");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect("the body it promised");
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn post(reader: &mut BufReader<TcpStream>, body: &str) -> Answer {
    send(reader, "POST", PATH, TOKEN, None, None, Some(body))
}

fn send(
    reader: &mut BufReader<TcpStream>,
    method: &str,
    path: &str,
    token: &str,
    origin: Option<&str>,
    session: Option<&str>,
    body: Option<&str>,
) -> Answer {
    let body = body.unwrap_or("");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    if let Some(session) = session {
        request.push_str(&format!("{SESSION_HEADER}: {session}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let mut stream = reader.get_ref();
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    stream.flush().expect("flushed");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    read_answer(reader)
}

fn connect(addr: SocketAddr) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(addr).expect("the endpoint accepts");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    BufReader::new(stream)
}

#[test]
fn a_client_can_name_itself_list_the_tools_and_call_one() {
    let endpoint = endpoint();
    let mut client = connect(endpoint.addr());

    let hello = post(
        &mut client,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"a-test-client"}}}"#,
    );
    assert_eq!(hello.status, 200, "{}", hello.body);
    let hello = hello.json();
    assert_eq!(hello["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(hello["result"]["serverInfo"]["name"], "colab-playground");

    // The id comes back in a header, and the client repeats it.
    let listed = post(
        &mut client,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let names: Vec<String> = listed.json()["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name").to_owned())
        .collect();
    assert_eq!(names.len(), 22, "{names:?}");
    assert!(names.contains(&"sessions_list".to_owned()), "{names:?}");
    assert!(names.contains(&"session_start".to_owned()), "{names:?}");
    assert!(names.contains(&"session_watch".to_owned()), "{names:?}");

    let called = post(
        &mut client,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sessions_list"}}"#,
    );
    let called = called.json();
    assert_eq!(called["result"]["isError"], false, "{called}");
    let text = called["result"]["content"][0]["text"]
        .as_str()
        .expect("a text block");
    let answer: serde_json::Value = serde_json::from_str(text).expect("the tool's JSON");
    assert_eq!(answer["sessions"][0]["name"], "playground-1");
    assert_eq!(answer["sessions"][0]["hardware"], "T4");
    assert!(!text.contains(TOKEN), "a tool never echoes the token");
}

#[test]
fn an_agent_drives_the_app_over_a_socket() {
    let recorder = Arc::new(Recorder::default());
    let endpoint = endpoint_with(Arc::clone(&recorder) as Arc<dyn Actions>);
    let mut client = connect(endpoint.addr());

    let hello = post(
        &mut client,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"a-driving-agent","version":"9"}}}"#,
    );
    let session = hello
        .headers
        .get(&SESSION_HEADER.to_ascii_lowercase())
        .expect("the session id comes back in a header")
        .clone();

    let called = send(
        &mut client,
        "POST",
        PATH,
        TOKEN,
        None,
        Some(&session),
        Some(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_stop","arguments":{"session":"playground-1"}}}"#,
        ),
    );
    let called = called.json();
    assert_eq!(called["result"]["isError"], false, "{called}");
    let text = called["result"]["content"][0]["text"]
        .as_str()
        .expect("a text block");
    let answer: serde_json::Value = serde_json::from_str(text).expect("the tool's JSON");
    assert_eq!(answer["done"], "session_stop");
    assert_eq!(
        recorder.asked(),
        [(
            Action::SessionStop {
                session: "playground-1".to_owned()
            },
            "a-driving-agent".to_owned()
        )],
        "the app was asked, and told which client asked"
    );

    // A tool that is not in the table stays a protocol fault rather than becoming a result.
    let unknown = post(
        &mut client,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"session_launch"}}"#,
    );
    assert_eq!(
        unknown.json()["error"]["code"],
        protocol::METHOD_NOT_FOUND,
        "{}",
        unknown.body
    );
}

#[test]
fn a_session_the_endpoint_handed_out_is_repeated_back() {
    let endpoint = endpoint();
    let mut client = connect(endpoint.addr());
    let hello = post(
        &mut client,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
    );
    let session = hello
        .headers
        .get(&SESSION_HEADER.to_ascii_lowercase())
        .cloned()
        .expect("a session id on initialize");
    assert_eq!(hello.json()["result"]["protocolVersion"], "2025-03-26");

    // A request that repeats it is served; one that names a session nobody issued is not.
    let repeated = send(
        &mut client,
        "POST",
        PATH,
        TOKEN,
        None,
        Some(&session),
        Some(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#),
    );
    assert_eq!(repeated.status, 200, "{}", repeated.body);
    let unknown = {
        let mut stream = client.get_ref();
        let body = r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#;
        let request = format!(
            "POST {PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\
             Mcp-Session-Id: 0123456789abcdef\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("written");
        stream.flush().expect("flushed");
        read_answer(&mut client)
    };
    assert_eq!(unknown.status, 404, "{}", unknown.body);
}

#[test]
fn the_endpoint_refuses_a_wrong_token_a_foreign_origin_and_another_path() {
    let endpoint = endpoint();
    let mut client = connect(endpoint.addr());
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    assert_eq!(
        send(&mut client, "POST", PATH, "wrong", None, None, Some(body)).status,
        401
    );
    assert_eq!(
        send(
            &mut client,
            "POST",
            PATH,
            TOKEN,
            Some("http://a-page.example"),
            None,
            Some(body)
        )
        .status,
        403,
        "a page in a browser may not reach it"
    );
    assert_eq!(
        send(
            &mut client,
            "POST",
            "/elsewhere",
            TOKEN,
            None,
            None,
            Some(body)
        )
        .status,
        404
    );
    assert_eq!(
        send(&mut client, "GET", PATH, TOKEN, None, None, None).status,
        405,
        "nothing is sent on its own yet"
    );
}

#[test]
fn a_request_the_endpoint_cannot_read_is_a_status_and_the_listener_stays_up() {
    let endpoint = endpoint();
    let mut client = connect(endpoint.addr());
    {
        let mut stream = client.get_ref();
        stream
            .write_all(b"nonsense\r\n\r\n")
            .expect("the nonsense is written");
        stream.flush().expect("flushed");
    }
    let refused = read_answer(&mut client);
    assert_eq!(refused.status, 400, "{}", refused.body);

    // The same listener still answers somebody else.
    let mut second = connect(endpoint.addr());
    let answered = post(&mut second, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert_eq!(answered.status, 200, "{}", answered.body);
}

#[test]
fn stopping_the_endpoint_frees_the_port() {
    let mut endpoint = endpoint();
    let addr = endpoint.addr();
    endpoint.stop();
    // Binding it again is the proof that nothing is left listening.
    let again = TcpListener::bind(addr).expect("the port is free after stop");
    drop(again);
}

#[test]
fn get_with_event_stream_receives_sse_notifications() {
    let endpoint = endpoint();
    let mut reader = connect(endpoint.addr());
    {
        let mut stream = reader.get_ref();
        let req = format!(
            "GET {PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\
             Accept: text/event-stream\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).expect("written");
        stream.flush().expect("flushed");
    }
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    assert!(status_line.contains("200"), "{status_line}");
    let mut content_type = String::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("header");
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.eq_ignore_ascii_case("content-type")
        {
            content_type = v.trim().to_ascii_lowercase();
        }
    }
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    endpoint.notify(
        "guard_warning",
        serde_json::json!({ "session": "playground-1" }),
    );

    let mut data_line = None;
    for _ in 0..10 {
        let mut line = String::new();
        reader.read_line(&mut line).expect("sse line");
        let trimmed = line.trim_end();
        if let Some(payload) = trimmed.strip_prefix("data: ") {
            data_line = Some(payload.to_owned());
            break;
        }
    }
    let payload = data_line.expect("an SSE data line");
    let msg: serde_json::Value = serde_json::from_str(&payload).expect("SSE JSON");
    assert_eq!(msg["jsonrpc"], "2.0");
    assert_eq!(msg["method"], "notifications/message");
    assert_eq!(msg["params"]["data"]["event"], "guard_warning");
    assert_eq!(msg["params"]["data"]["detail"]["session"], "playground-1");
}

#[test]
fn get_event_stream_enforces_auth_origin_and_session_rules() {
    let endpoint = endpoint();
    let sse = |token: Option<&str>, origin: Option<&str>, session: Option<&str>| -> Answer {
        let mut reader = connect(endpoint.addr());
        let mut req = format!("GET {PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n");
        if let Some(token) = token {
            req.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(origin) = origin {
            req.push_str(&format!("Origin: {origin}\r\n"));
        }
        if let Some(session) = session {
            req.push_str(&format!("{SESSION_HEADER}: {session}\r\n"));
        }
        req.push_str("\r\n");
        let mut stream = reader.get_ref();
        stream.write_all(req.as_bytes()).expect("written");
        stream.flush().expect("flushed");
        read_answer(&mut reader)
    };

    assert_eq!(sse(None, None, None).status, 401);
    assert_eq!(sse(Some("wrong"), None, None).status, 401);
    assert_eq!(
        sse(Some(TOKEN), Some("http://a-page.example"), None).status,
        403
    );
    assert_eq!(
        sse(Some(TOKEN), None, Some("0123456789abcdef")).status,
        404
    );
}
