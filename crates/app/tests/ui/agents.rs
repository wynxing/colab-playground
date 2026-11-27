//! Settings → Agents: what it shows while the endpoint is off, what switching it on does, the
//! token's two states, and an agent driving the app through the endpoint. The protocol itself is
//! core's and is tested there; these tests are about the window.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use colab_playground::{
    route::Route,
    settings::AppearancePref,
    ui::{
        overlays::{APPROVAL_ALLOW, APPROVAL_REFUSE},
        pages::settings::{
            AGENTS_ADDRESS_SELECTOR, AGENTS_CONFIG_SELECTOR, AGENTS_CONFIRM_SELECTOR,
            AGENTS_TOGGLE_SELECTOR, AGENTS_TOKEN_HIDDEN_SELECTOR, AGENTS_TOKEN_REVEAL_SELECTOR,
            AGENTS_TOKEN_ROTATE_SELECTOR, AGENTS_TOKEN_SHOWN_SELECTOR,
        },
    },
};
use colab_playground_core::{
    mock::{MockActiveRun, MockFailures, MockScenario},
    plugins::{LocalizedText, ResolvedEndpoint},
    tunnel::{TunnelEvent, TunnelState},
    vm::RunState,
};
use gpui::TestAppContext;
use serde_json::{Value, json};

use crate::support::{ConfigDir, Options, TestApp};

/// Settings that switch the endpoint on with a port the system picks, so two runs of these tests
/// cannot collide on a fixed one.
const ON: &str = r#"{"mcp_enabled":true,"mcp_port":0}"#;

/// The name an agent gives itself when it connects here.
const AGENT: &str = "openbot";

fn open_settings(cx: &mut TestAppContext, options: Options) -> TestApp<'_> {
    let mut app = TestApp::with(cx, options);
    app.click(Route::Settings.nav_selector());
    assert_eq!(app.route(), Route::Settings);
    app
}

fn with_settings<'a>(cx: &'a mut TestAppContext, text: &str) -> TestApp<'a> {
    let options = Options {
        config: ConfigDir::TempWith(text.to_owned()),
        ..Options::default()
    };
    open_settings(cx, options)
}

/// The endpoint on, and whatever `scenario` puts on the accounts and their runtimes.
fn with_agents<'a>(cx: &'a mut TestAppContext, scenario: MockScenario) -> TestApp<'a> {
    with_agents_and(cx, scenario, MockFailures::default())
}

/// The same, on a port of its own: a port the system picks is resolved again while the app runs and
/// the endpoint is bound again, which is fine for the app but awkward for a test that holds a
/// connection across a run starting.
fn with_agents_on<'a>(
    cx: &'a mut TestAppContext,
    scenario: MockScenario,
    port: u16,
) -> TestApp<'a> {
    let options = Options {
        config: ConfigDir::TempWith(format!("{{\"mcp_enabled\":true,\"mcp_port\":{port}}}")),
        ..Options::scenario(scenario)
    };
    open_settings(cx, options)
}
/// The same, with the backend's failures switched on before the app starts.
fn with_agents_and<'a>(
    cx: &'a mut TestAppContext,
    scenario: MockScenario,
    failures: MockFailures,
) -> TestApp<'a> {
    let options = Options {
        config: ConfigDir::TempWith(ON.to_owned()),
        failures,
        ..Options::scenario(scenario)
    };
    open_settings(cx, options)
}

/// What an agent connects with: where it listens and what it has to bring.
fn endpoint(app: &mut TestApp<'_>) -> (SocketAddr, String) {
    app.read(|state, _| {
        let agents = state.agents();
        (
            agents.address().expect("the endpoint listens"),
            agents.token().expect("a token").to_owned(),
        )
    })
}

/// The runtimes the window is showing, by name.
fn runtime_names(app: &mut TestApp<'_>) -> Vec<String> {
    app.read(|state, _| {
        state
            .runtimes()
            .ready()
            .map(|view| {
                view.runtimes
                    .iter()
                    .filter_map(|runtime| runtime.id.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Drives the window until `done` holds. The endpoint answers on its own thread, so a call reaches
/// the window some time after the request was written, and the window has to be run again for it.
fn settle_until(app: &mut TestApp<'_>, mut done: impl FnMut(&mut TestApp<'_>) -> bool) {
    for _ in 0..800 {
        if done(app) {
            return;
        }
        app.advance(Duration::from_millis(5));
    }
    panic!("the window never got there");
}

/// Runs the window a few more times, for an answer that changes nothing on screen: what a call
/// turned down leaves behind is a tool result, not a frame.
fn settle_a_while(app: &mut TestApp<'_>) {
    for _ in 0..40 {
        app.advance(Duration::from_millis(5));
    }
}

/// A client on the endpoint, the way a real one is: it introduces itself, repeats the session it
/// was given, and calls tools. A write is answered only once the window has run, so writing and
/// reading are separate steps.
struct Agent {
    reader: BufReader<TcpStream>,
    token: String,
    session: String,
    next: u64,
}

impl Agent {
    fn connect(address: SocketAddr, token: &str) -> Agent {
        let stream = TcpStream::connect(address).expect("the endpoint accepts");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("a read timeout");
        let mut agent = Agent {
            reader: BufReader::new(stream),
            token: token.to_owned(),
            session: String::new(),
            next: 1,
        };
        let hello = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"openbot"}}}"#.to_owned();
        agent.send(&hello);
        let hello = agent.read();
        assert_eq!(hello.0, 200, "{}", hello.2);
        agent.session = hello
            .1
            .get("mcp-session-id")
            .expect("an id the client repeats")
            .clone();
        agent
    }

    /// Calls a tool. The answer is read separately: a write is held open while the window does it.
    fn call(&mut self, name: &str, arguments: Value) {
        let id = self.next;
        self.next += 1;
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
        .to_string();
        self.send(&body);
    }

    fn send(&mut self, body: &str) {
        let mut request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n",
            self.token,
            body.len()
        );
        if !self.session.is_empty() {
            request.push_str(&format!("Mcp-Session-Id: {}\r\n", self.session));
        }
        request.push_str("\r\n");
        request.push_str(body);
        let mut stream = self.reader.get_ref();
        stream.write_all(request.as_bytes()).expect("written");
        stream.flush().expect("flushed");
    }

    /// Status, headers and body of one response.
    fn read(&mut self) -> (u16, HashMap<String, String>, String) {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("a status line");
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status in {line:?}"));
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            self.reader.read_line(&mut header).expect("a header");
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
        self.reader.read_exact(&mut body).expect("the body");
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// The tool result of the call it just made: whether it was an error, and its body. The body of
    /// an error is core's own machine-readable error, which is what a client acts on.
    fn result(&mut self) -> (bool, Value) {
        let (status, _, body) = self.read();
        assert_eq!(status, 200, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("not JSON: {e}"));
        let is_error = answer["result"]["isError"]
            .as_bool()
            .unwrap_or_else(|| panic!("no tool result: {answer}"));
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text: {answer}"));
        let body: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON: {e}"));
        (is_error, body)
    }
}

struct SseClient {
    reader: BufReader<TcpStream>,
}

impl SseClient {
    fn connect(address: SocketAddr, token: &str, session: &str) -> Self {
        let mut stream = TcpStream::connect(address).expect("SSE endpoint accepts");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read timeout");
        let req = format!(
            "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
             Accept: text/event-stream\r\nMcp-Session-Id: {session}\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).expect("SSE request");
        stream.flush().expect("flushed");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).expect("SSE status line");
        assert!(status.contains("200"), "{status}");
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("SSE header");
            if header.trim_end().is_empty() {
                break;
            }
        }
        Self { reader }
    }

    fn next_matching(&mut self, event: &str, session: Option<&str>) -> Value {
        for _ in 0..40 {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("SSE line");
            let Some(payload) = line.trim_end().strip_prefix("data: ") else {
                continue;
            };
            let msg: Value = serde_json::from_str(payload).expect("SSE notification JSON");
            let data = &msg["params"]["data"];
            if data["event"].as_str() != Some(event) {
                continue;
            }
            if let Some(expected_session) = session
                && data["detail"]["session"].as_str() != Some(expected_session)
            {
                continue;
            }
            return msg;
        }
        panic!("timed out waiting for SSE event {event:?} for {session:?}");
    }
}

#[gpui::test]
fn the_section_is_there_and_listens_to_nothing_until_it_is_switched_on(cx: &mut TestAppContext) {
    let mut app = open_settings(cx, Options::default());
    assert!(app.shown(AGENTS_TOGGLE_SELECTOR));
    assert!(!app.read(|state, _| state.agents_enabled()));
    assert!(app.read(|state, _| state.agents().address().is_none()));
    // Nothing to show or copy while it is off.
    assert!(!app.shown(AGENTS_ADDRESS_SELECTOR));
    assert!(!app.shown(AGENTS_TOKEN_SHOWN_SELECTOR));
    assert!(!app.shown(AGENTS_CONFIG_SELECTOR));
}

#[gpui::test]
fn switching_it_on_binds_loopback_saves_it_and_switching_it_off_frees_the_port(
    cx: &mut TestAppContext,
) {
    // On a port the system picks: the app on this machine may well hold the default one, and what
    // this test is about is the switch binding at all, not which port it lands on.
    let mut app = with_settings(cx, r#"{"mcp_port":0}"#);

    app.click(AGENTS_TOGGLE_SELECTOR);
    app.settle();
    assert!(app.read(|state, _| state.agents_enabled()));
    let address = app
        .read(|state, _| state.agents().address())
        .expect("it listens after being switched on");
    assert!(address.ip().is_loopback(), "{address} is not loopback");
    assert!(app.shown(AGENTS_ADDRESS_SELECTOR));
    assert_eq!(
        app.saved_settings().map(|settings| settings.mcp_enabled),
        Some(true),
        "the switch is a setting, so it survives a restart"
    );

    app.click(AGENTS_TOGGLE_SELECTOR);
    app.settle();
    assert!(!app.read(|state, _| state.agents_enabled()));
    assert!(app.read(|state, _| state.agents().address().is_none()));
    assert!(
        std::net::TcpListener::bind(address).is_ok(),
        "a stopped endpoint holds no port"
    );
}

#[gpui::test]
fn the_token_is_hidden_until_it_is_asked_for_and_replacing_it_makes_a_new_one(
    cx: &mut TestAppContext,
) {
    let mut app = with_settings(cx, ON);
    let first = app
        .read(|state, _| state.agents().token().map(str::to_owned))
        .expect("an endpoint that was asked for has a token");

    assert!(
        app.shown(AGENTS_TOKEN_HIDDEN_SELECTOR),
        "hidden to begin with"
    );
    assert!(!app.shown(AGENTS_TOKEN_SHOWN_SELECTOR));

    app.click(AGENTS_TOKEN_REVEAL_SELECTOR);
    assert!(
        app.shown(AGENTS_TOKEN_SHOWN_SELECTOR),
        "shown once asked for"
    );
    assert!(!app.shown(AGENTS_TOKEN_HIDDEN_SELECTOR));

    // What an agent needs is on the page: where to connect and what to bring.
    assert!(app.shown(AGENTS_ADDRESS_SELECTOR));
    assert!(app.shown(AGENTS_CONFIG_SELECTOR));

    app.click(AGENTS_TOKEN_ROTATE_SELECTOR);
    app.settle();
    let second = app
        .read(|state, _| state.agents().token().map(str::to_owned))
        .expect("a token after replacing");
    assert_ne!(
        second, first,
        "replacing it stops an agent with the old one"
    );
}

#[gpui::test]
fn an_agent_reads_the_app_over_the_socket(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("sessions_list", json!({}));
    let (is_error, listed) = agent.result();
    assert!(!is_error, "{listed}");
    let names: Vec<&str> = listed["sessions"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|session| session["name"].as_str())
        .collect();
    assert!(names.contains(&"t4-91c2"), "{listed}");

    // A read asks the window for nothing and changes nothing on it.
    assert!(app.read(|state, _| state.agent_approval().is_none()));
    assert!(runtime_names(&mut app).contains(&"t4-91c2".to_owned()));
}

#[gpui::test]
fn a_paid_call_parks_the_window_and_runs_when_the_user_allows_it(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "session_extend",
        json!({ "session": "t4-91c2", "minutes": 30 }),
    );
    settle_until(&mut app, |app| app.shown(APPROVAL_ALLOW));

    // The question names the action, its target and the client that asked, and nothing has started
    // while it stands.
    let prompt = app
        .read(|state, _| state.agent_approval().cloned())
        .expect("a question is up");
    assert_eq!(prompt.action, "session_extend");
    assert_eq!(prompt.target, "t4-91c2");
    assert_eq!(prompt.client, AGENT);
    assert!(
        prompt.units_per_hour.is_some(),
        "a runtime that is running has a rate to show"
    );
    assert!(runtime_names(&mut app).contains(&"t4-91c2".to_owned()));

    app.click(APPROVAL_ALLOW);
    assert!(
        !app.shown(APPROVAL_ALLOW),
        "the question goes with the answer"
    );
    settle_a_while(&mut app);

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["session"], "t4-91c2");
    assert_eq!(answer["minutes"], 30);
    assert_eq!(answer["auto_stop"], "scheduled");
    // Moving the stop does not close the runtime: the call took the path a click takes.
    assert!(runtime_names(&mut app).contains(&"t4-91c2".to_owned()));
}

/// Stopping saves money, so it does not wait for a click. A dialog here would leave the runtime
/// billing for as long as the question stood.
#[gpui::test]
fn stopping_a_runtime_does_not_ask(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("session_stop", json!({ "session": "t4-91c2" }));
    settle_until(&mut app, |app| {
        !runtime_names(app).contains(&"t4-91c2".to_owned())
    });

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["stopped"], true);
    assert!(app.read(|state, _| state.agent_approval().is_none()));
}
/// A `session_start` that names no session is the ordinary case — the tool's own description
/// says the app's defaults are used — so the app has to name it the way its own new-session form
/// does, instead of handing the backend an empty name that it refuses.
#[gpui::test]
fn a_start_with_no_name_is_named_by_the_app(cx: &mut TestAppContext) {
    // One account, no runtime: what starts here is the app's own doing.
    let mut app = with_agents(cx, MockScenario::single());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("session_start", json!({ "hardware": "T4" }));
    settle_until(&mut app, |app| app.shown(APPROVAL_ALLOW));
    app.click(APPROVAL_ALLOW);
    settle_until(&mut app, |app| !runtime_names(app).is_empty());

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    let name = answer["session"]
        .as_str()
        .expect("a session name")
        .to_owned();
    assert!(name.starts_with("t4-"), "named for its hardware: {answer}");
    assert_eq!(
        runtime_names(&mut app),
        vec![name],
        "the runtime the app started is the one it answered with: {answer}"
    );
}
/// A plugin that keeps a server running never finishes, so an answer held until it does would
/// never come: the agent is answered at the first stage the plugin publishes, and the run carries
/// on. Without this the client times out at 300s with the run working perfectly.
#[gpui::test]
fn a_run_is_answered_at_its_first_stage_and_keeps_running(cx: &mut TestAppContext) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8799);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "plugin_run",
        json!({ "plugin": "comfyui", "session": "t4-91c2", "inputs": { "preset": "sd15" } }),
    );
    settle_until(&mut app, |app| {
        app.read(|state, _| state.plugin_run().is_some_and(|run| run.status.is_some()))
    });
    assert!(
        app.read(|state, _| state.agent_approval().is_none()),
        "a plugin on a runtime that is already open does not ask"
    );

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert!(
        answer["run"]["stage"].is_string(),
        "the answer names the stage the run reached: {answer}"
    );
    // The last stage of ComfyUI's run is `warmup`: an answer that only came when the run was over
    // would name that one, and a client waiting for it would time out first.
    assert_ne!(
        answer["run"]["stage"], "warmup",
        "the answer is the first stage the plugin published, not the last: {answer}"
    );
}
/// A watch with nothing to report answers anyway: the wait is always inside a client's own
/// timeout, which is the whole point of it.
#[gpui::test]
fn a_watch_answers_with_the_run_as_it_stands_when_nothing_happens(cx: &mut TestAppContext) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8800);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "plugin_run",
        json!({ "plugin": "comfyui", "session": "t4-91c2", "inputs": { "preset": "sd15" } }),
    );
    settle_until(&mut app, |app| {
        app.read(|state, _| state.plugin_run().is_some())
    });
    let _ = agent.result();

    agent.call("run_watch", json!({ "plugin": "comfyui" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching())
    });
    app.advance(Duration::from_secs(121));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching())
    });

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["changed"], "none", "{answer}");
    assert!(answer["run"]["stage"].is_string(), "{answer}");
}

/// One watch at a time: a second client asking while the first waits is told so, and the first
/// keeps waiting.
#[gpui::test]
fn a_second_watch_is_told_to_wait_its_turn(cx: &mut TestAppContext) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8803);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "plugin_run",
        json!({ "plugin": "comfyui", "session": "t4-91c2", "inputs": { "preset": "sd15" } }),
    );
    settle_until(&mut app, |app| {
        app.read(|state, _| state.plugin_run().is_some())
    });
    let _ = agent.result();

    let mut first = Agent::connect(address, &token);
    first.call("run_watch", json!({ "plugin": "comfyui" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching())
    });

    let mut second = Agent::connect(address, &token);
    second.call("run_watch", json!({ "plugin": "comfyui" }));
    settle_a_while(&mut app);

    let (is_error, answer) = second.result();
    assert!(is_error, "{answer}");
    assert_eq!(answer["kind"], "denied");
    assert_eq!(answer["params"]["reason"], "busy");
    assert!(
        app.read(|state, _| state.agent_is_watching()),
        "the first watch is still waiting"
    );
}
/// A watch on a plugin that is not running is answered rather than waited on.
#[gpui::test]
fn a_watch_on_a_plugin_with_no_run_is_answered(cx: &mut TestAppContext) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8802);
    let (address, token) = endpoint(&mut app);
    settle_a_while(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("run_watch", json!({ "plugin": "llama-cpp" }));
    // The window takes a call off the endpoint's queue on its own tick, which a test drives.
    settle_a_while(&mut app);

    let (is_error, answer) = agent.result();
    assert!(is_error, "nothing is running it: {answer}");
    assert_eq!(answer["kind"], "plugin_not_found");
}
#[gpui::test]
fn a_refused_call_starts_nothing_and_is_told_so(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "session_extend",
        json!({ "session": "t4-91c2", "minutes": 0 }),
    );
    settle_until(&mut app, |app| app.shown(APPROVAL_REFUSE));

    app.click(APPROVAL_REFUSE);
    assert!(!app.shown(APPROVAL_REFUSE));
    settle_a_while(&mut app);

    let (is_error, answer) = agent.result();
    assert!(is_error, "a refusal is an error result: {answer}");
    assert_eq!(answer["kind"], "denied");
    assert_eq!(answer["params"]["action"], "session_extend");
    assert_eq!(answer["params"]["reason"], "refused");
    assert_eq!(answer["next_step"], "none", "asking again is wrong");
    assert!(
        runtime_names(&mut app).contains(&"t4-91c2".to_owned()),
        "cancelling the auto-stop did not happen"
    );
}

#[gpui::test]
fn a_second_paid_call_is_refused_while_one_waits(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut first = Agent::connect(address, &token);
    first.call(
        "session_extend",
        json!({ "session": "t4-91c2", "minutes": 30 }),
    );
    settle_until(&mut app, |app| app.shown(APPROVAL_ALLOW));

    // The window asks about one thing at a time, so the second call is answered rather than queued
    // behind the first.
    let mut second = Agent::connect(address, &token);
    second.call("session_start", json!({ "hardware": "T4" }));
    settle_a_while(&mut app);

    let (is_error, answer) = second.result();
    assert!(is_error, "{answer}");
    assert_eq!(answer["kind"], "denied");
    assert_eq!(answer["params"]["action"], "session_start");
    assert_eq!(answer["params"]["reason"], "busy");

    // The first is still the one being asked about, and answering it still works.
    let prompt = app
        .read(|state, _| state.agent_approval().cloned())
        .expect("the first question is still up");
    assert_eq!(prompt.action, "session_extend");
    app.click(APPROVAL_REFUSE);
    settle_a_while(&mut app);
    assert!(first.result().0, "the first call ends as refused too");
}

#[gpui::test]
fn a_write_tool_that_costs_nothing_does_what_a_click_does(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "settings_set",
        json!({ "changes": { "appearance": "dark" } }),
    );
    settle_until(&mut app, |app| {
        app.read(|state, _| state.settings().appearance) == AppearancePref::Dark
    });

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["settings"]["appearance"], "dark");
    // Nothing was asked about: only what spends money is.
    assert!(app.read(|state, _| state.agent_approval().is_none()));
    assert_eq!(
        app.saved_settings().map(|settings| settings.appearance),
        Some(AppearancePref::Dark),
        "the change ends in the same save a click ends in"
    );
}

#[gpui::test]
fn a_target_the_app_does_not_have_is_answered_rather_than_waited_on(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("session_stop", json!({ "session": "not-a-runtime" }));
    settle_a_while(&mut app);

    let (is_error, answer) = agent.result();
    assert!(is_error, "{answer}");
    assert_eq!(answer["kind"], "session_gone");
    assert_eq!(answer["params"]["session"], "not-a-runtime");
    // A session that is not there is not something to ask the user about.
    assert!(app.read(|state, _| state.agent_approval().is_none()));
}

/// A `plugin_run` that names no session opens a runtime, so it asks. Refusing it starts nothing.
#[gpui::test]
fn a_plugin_run_that_would_open_a_runtime_asks_first(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);
    let before = runtime_names(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "plugin_run",
        json!({ "plugin": "comfyui", "inputs": { "preset": "sd15" } }),
    );
    settle_until(&mut app, |app| app.shown(APPROVAL_ALLOW));
    let prompt = app
        .read(|state, _| state.agent_approval().cloned())
        .expect("opening a runtime asks");
    assert_eq!(prompt.action, "plugin_run");

    app.click(APPROVAL_REFUSE);
    settle_a_while(&mut app);
    let (is_error, answer) = agent.result();
    assert!(is_error, "{answer}");
    assert_eq!(answer["params"]["reason"], "refused");
    assert_eq!(runtime_names(&mut app), before);
    assert!(app.read(|state, _| state.plugin_run().is_none()));
}

/// The question is a setting. Off, a call that would otherwise ask runs at once.
#[gpui::test]
fn a_start_does_not_ask_when_asking_is_switched_off(cx: &mut TestAppContext) {
    let options = Options {
        config: ConfigDir::TempWith(
            r#"{"mcp_enabled":true,"mcp_port":0,"mcp_confirm":false}"#.to_owned(),
        ),
        ..Options::scenario(MockScenario::single())
    };
    let mut app = open_settings(cx, options);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("session_start", json!({ "hardware": "T4" }));
    settle_until(&mut app, |app| !runtime_names(app).is_empty());

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert!(
        answer["session"].as_str().unwrap_or("").starts_with("t4-"),
        "{answer}"
    );
    assert!(app.read(|state, _| state.agent_approval().is_none()));
}

#[gpui::test]
fn asking_before_spending_is_on_until_it_is_switched_off(cx: &mut TestAppContext) {
    let mut app = open_settings(cx, Options::default());
    assert!(app.shown(AGENTS_CONFIRM_SELECTOR));
    assert!(app.read(|state, _| state.agents_confirm()));

    app.click(AGENTS_CONFIRM_SELECTOR);
    app.settle();
    assert!(!app.read(|state, _| state.agents_confirm()));
    assert_eq!(
        app.saved_settings().map(|settings| settings.mcp_confirm.0),
        Some(false),
        "the switch is a setting, so it survives a restart"
    );
}

/// The agent must not be able to turn the question off. The switch is the user's.
#[gpui::test]
fn an_agent_cannot_turn_the_question_off(cx: &mut TestAppContext) {
    let mut app = with_agents(cx, MockScenario::demo());
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call(
        "settings_set",
        json!({ "changes": { "ask_before_spend": false } }),
    );
    settle_a_while(&mut app);

    let (is_error, answer) = agent.result();
    assert!(is_error, "{answer}");
    assert!(app.read(|state, _| state.agents_confirm()));
}

#[gpui::test]
fn run_watch_follows_a_reattached_run_over_the_socket(cx: &mut TestAppContext) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8804);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    let mut sse = SseClient::connect(address, &token, &agent.session);

    app.mock.seed_active_run(MockActiveRun {
        account: 0,
        session: "a100-7f3a".into(),
        run_id: "reattach-watch-1".into(),
        plugin: "comfyui".into(),
        stage: Some("download".into()),
        state: RunState::Running,
        log_tail: vec!["resumed line".into()],
        keep_alive: true,
        endpoints: vec![ResolvedEndpoint {
            id: "ui".into(),
            remote_port: 8188,
            path: "/".into(),
            health: Some("/".into()),
            kind: "comfyui".into(),
            label: LocalizedText::default(),
        }],
        manual_stages: false,
    });
    app.update(|state, cx| state.load_runtimes(cx));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.plugin_run().is_some())
    });

    agent.call("run_status", json!({ "plugin": "comfyui" }));
    settle_a_while(&mut app);
    let (is_error, status) = agent.result();
    assert!(!is_error, "{status}");
    assert_eq!(status["session"], "a100-7f3a");
    assert_eq!(status["ready"], true);

    let ready_note = sse.next_matching("run_ready", None);
    assert_eq!(ready_note["params"]["data"]["detail"]["run"]["ready"], true);

    agent.call("run_watch", json!({ "plugin": "comfyui" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching())
    });
    app.advance(Duration::from_secs(121));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching())
    });
    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["changed"], "none", "{answer}");
    assert_eq!(answer["run"]["session"], "a100-7f3a", "{answer}");
    assert_eq!(answer["run"]["ready"], true, "{answer}");

    app.update(|state, cx| state.cancel_run(cx));
    settle_a_while(&mut app);
}

#[gpui::test]
fn session_watch_reports_guard_warning_auto_stop_countdown_and_sse(cx: &mut TestAppContext) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8805);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    let mut sse = SseClient::connect(address, &token, &agent.session);

    agent.call("session_watch", json!({ "session": "a100-7f3a" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching_session())
    });

    // `a100-7f3a` starts at elapsed 10 min (limit 120 min). +90 min -> 100 min elapsed (>= 80% warn)
    // and 20 min remaining (> 10 min auto_stop_soon).
    app.advance(Duration::from_secs(90 * 60));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching_session())
    });

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["event"], "guard_warning", "{answer}");
    assert_eq!(answer["session"], "a100-7f3a", "{answer}");
    assert!(
        !answer["warnings"].as_array().expect("warnings").is_empty(),
        "{answer}"
    );
    let warn_note = sse.next_matching("guard_warning", Some("a100-7f3a"));
    assert_eq!(warn_note["params"]["level"], "warning");

    agent.call("session_watch", json!({ "session": "a100-7f3a" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching_session())
    });

    // +12 min -> 8 min remaining (<= 10 min AUTO_STOP_SOON), same runtime limit warning kind stays deduplicated.
    app.advance(Duration::from_secs(12 * 60));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching_session())
    });

    let (is_error, answer) = agent.result();
    assert!(!is_error, "{answer}");
    assert_eq!(answer["event"], "auto_stop_soon", "{answer}");
    assert_eq!(answer["session"], "a100-7f3a", "{answer}");
    assert!(
        answer["detail"]["remaining_seconds"]
            .as_u64()
            .is_some_and(|s| s <= 600),
        "{answer}"
    );
    let soon_note = sse.next_matching("auto_stop_soon", Some("a100-7f3a"));
    assert_eq!(soon_note["params"]["level"], "warning");
}

#[gpui::test]
fn session_watch_reports_tunnel_disconnect_unhealthy_timeout_and_missing_session(
    cx: &mut TestAppContext,
) {
    let mut app = with_agents_on(cx, MockScenario::demo(), 8806);
    let (address, token) = endpoint(&mut app);

    let mut agent = Agent::connect(address, &token);
    agent.call("session_watch", json!({ "session": "nope" }));
    settle_a_while(&mut app);
    let (is_error, missing) = agent.result();
    assert!(is_error, "{missing}");
    assert_eq!(missing["kind"], "session_gone", "{missing}");

    agent.call(
        "forward_open",
        json!({ "session": "a100-7f3a", "remote_port": 8188 }),
    );
    settle_until(&mut app, |app| app.read(|state, _| state.tunnels().next().is_some()));
    let (is_error, fwd) = agent.result();
    assert!(!is_error, "{fwd}");
    let tunnel_id = app
        .read(|state, _| state.tunnels().next().map(|t| t.id))
        .expect("tunnel id");

    agent.call("session_watch", json!({ "session": "a100-7f3a" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching_session())
    });
    app.mock.emit_tunnel_event(TunnelEvent::State {
        id: tunnel_id,
        state: TunnelState::Reconnecting {
            attempt: 1,
            delay_secs: 2,
        },
    });
    app.advance(Duration::from_millis(300));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching_session())
    });
    let (is_error, disconnected) = agent.result();
    assert!(!is_error, "{disconnected}");
    assert_eq!(disconnected["event"], "tunnel_disconnected", "{disconnected}");
    assert_eq!(disconnected["detail"]["state"], "reconnecting");

    agent.call("session_watch", json!({ "session": "a100-7f3a" }));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching_session())
    });
    app.mock.emit_tunnel_event(TunnelEvent::Health {
        id: tunnel_id,
        healthy: false,
    });
    app.advance(Duration::from_millis(300));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching_session())
    });
    let (is_error, unhealthy) = agent.result();
    assert!(!is_error, "{unhealthy}");
    assert_eq!(unhealthy["event"], "tunnel_unhealthy", "{unhealthy}");
    assert_eq!(unhealthy["detail"]["healthy"], false);

    // Busy refusal + timeout answer.
    let mut first = Agent::connect(address, &token);
    first.call("session_watch", json!({}));
    settle_until(&mut app, |app| {
        app.read(|state, _| state.agent_is_watching_session())
    });

    let mut second = Agent::connect(address, &token);
    second.call("session_watch", json!({ "session": "a100-7f3a" }));
    settle_a_while(&mut app);
    let (is_error, busy) = second.result();
    assert!(is_error, "{busy}");
    assert_eq!(busy["kind"], "denied", "{busy}");
    assert_eq!(busy["params"]["reason"], "busy", "{busy}");

    app.advance(Duration::from_secs(121));
    settle_until(&mut app, |app| {
        !app.read(|state, _| state.agent_is_watching_session())
    });
    let (is_error, lapsed) = first.result();
    assert!(!is_error, "{lapsed}");
    assert_eq!(lapsed["event"], "none", "{lapsed}");
}
