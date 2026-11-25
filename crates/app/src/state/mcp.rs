//! The local agent endpoint, as this app runs it.
//!
//! The endpoint itself is core's (`colab_playground_core::mcp`): it speaks the protocol, holds the
//! tool table and knows nothing about this app. What this app adds is here — a [`Source`], which is
//! a snapshot of the state the window renders, and the endpoint's life, which follows the settings.
//!
//! The snapshot sits behind a mutex the state publishes into, deliberately: the endpoint serves on
//! its own threads and must never reach into [`AppState`]. What is published is what an agent
//! reads, so an agent sees the same truth the interface shows instead of a second cache that can
//! drift away from it.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use colab_playground_core::{
    Error, ErrorKind, Result,
    backend::BackendKind,
    mcp::{
        Config, Endpoint,
        actions::Action,
        view::{
            Account, AppSettings, Forward, Node, Output, Plugin, Run, Session, Snapshot, Source,
            Usage, Workflow,
        },
    },
    plugins::PluginSource,
    usage::published_rate,
};
use serde_json::{Value, json};

use super::{
    AppState, Load, ProvisionPhase,
    agents::Bridge,
    runs::{RunPhase, outputs_dir},
};

/// How many lines of a run's log a tool answers with. Enough to see what a build is doing, short
/// enough that a tool result stays a result.
const LOG_TAIL_LINES: usize = 40;

/// The endpoint as the app holds it: where it listens, what went wrong last, and the snapshot it
/// answers from.
#[derive(Default)]
pub struct Agents {
    source: Arc<SnapshotSource>,
    endpoint: Option<Endpoint>,
    address: Option<SocketAddr>,
    /// The token the running endpoint demands, so rotating it restarts the endpoint.
    token: Option<String>,
    /// The executor the running endpoint puts calls on, so stopping it can answer the calls still
    /// waiting rather than leave them.
    bridge: Option<Arc<Bridge>>,
    error: Option<Error>,
}

impl Agents {
    /// Where an agent connects, while the endpoint is running.
    pub fn address(&self) -> Option<SocketAddr> {
        self.address
    }

    /// The token an agent has to send, kept after the endpoint stops: the window can still show
    /// what an agent would need before the user switches it back on.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Why the endpoint is not running, when it was asked to be and could not be.
    pub fn error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// Brings the endpoint in line with what the settings ask for: `Some((port, token))` to run,
    /// `None` to stop. Called with what is already true, it does nothing — which is why it is safe
    /// to call after every settings save.
    /// Answers the executor the window has to read, when the endpoint started. `None` means it did
    /// not: either the settings ask for it to be off, or it could not start.
    pub fn sync(&mut self, wanted: Option<(u16, String)>) -> Option<Arc<Bridge>> {
        let running_as_asked = match (&self.endpoint, &wanted) {
            (Some(endpoint), Some((port, token))) => {
                endpoint.addr().port() == *port && self.token.as_deref() == Some(token.as_str())
            }
            (None, None) => true,
            _ => false,
        };
        if running_as_asked {
            return None;
        }
        self.stop();
        let (port, token) = wanted?;
        // The app's own executor — the thing that carries an agent's call out through the same
        // path a click takes — is the endpoint's other half. The window drains its queue on the
        // foreground and answers there, which is what makes a tool result mean "it is done".
        let bridge = Bridge::new();
        match Endpoint::start(
            Config {
                token: token.clone(),
                port,
                actions: bridge.clone(),
            },
            Arc::clone(&self.source) as Arc<dyn Source>,
        ) {
            Ok(endpoint) => {
                self.address = Some(endpoint.addr());
                self.token = Some(token);
                self.endpoint = Some(endpoint);
                self.bridge = Some(Arc::clone(&bridge));
                self.error = None;
                Some(bridge)
            }
            // The port is taken, or the listener could not start: the window says so instead of
            // the app refusing to run.
            Err(error) => {
                self.address = None;
                self.token = Some(token);
                self.error = Some(error);
                None
            }
        }
    }

    /// Remembers why the endpoint could not be asked to start at all — a backend that would not
    /// answer with its token.
    pub fn failed(&mut self, error: Error) {
        self.error = Some(error);
    }

    /// Whether the endpoint is already what the settings ask for, so a save that changed
    /// something else does not restart it (and does not re-ask the backend for its token).
    pub fn matches(&self, wanted: Option<u16>) -> bool {
        match (self.endpoint.is_some(), wanted) {
            (false, None) => true,
            (true, Some(port)) => self.address.is_some_and(|address| address.port() == port),
            _ => false,
        }
    }

    /// What an agent reads until the state publishes again.
    pub(crate) fn publish(&self, snapshot: Snapshot) {
        // A poisoned lock means a publisher panicked; the endpoint keeps answering the last
        // snapshot rather than going silent, so the failure is the only thing to ignore.
        if let Ok(mut held) = self.source.0.lock() {
            *held = snapshot;
        }
    }

    fn stop(&mut self) {
        self.endpoint = None;
        self.address = None;
        // A call still waiting on the window is answered as dismissed rather than left to the
        // backstop: the endpoint it was made to is gone.
        if let Some(bridge) = self.bridge.take() {
            bridge.close();
        }
    }
}

/// The snapshot the endpoint answers from. Reaching into `AppState` is what this type exists to
/// avoid, so it holds nothing but the last published snapshot.
#[derive(Default)]
struct SnapshotSource(Mutex<Snapshot>);

impl Source for SnapshotSource {
    fn snapshot(&self) -> Snapshot {
        self.0.lock().map(|held| held.clone()).unwrap_or_default()
    }
}

/// Everything a read tool may answer, taken from the state the pages read.
pub(crate) fn snapshot_of(state: &AppState) -> Snapshot {
    Snapshot {
        sessions: sessions_of(state),
        usage: usage_of(state),
        runs: runs_of(state),
        plugins: plugins_of(state),
        accounts: accounts_of(state),
        forwards: forwards_of(state),
        settings: settings_of(state),
        notes: notes_of(state),
    }
}

/// What the app cannot answer yet. An agent reads these beside an empty list, so "the app has not
/// looked" is never mistaken for "nothing is there" — which, for sessions, is the difference
/// between doing nothing and starting something.
fn notes_of(state: &AppState) -> Vec<String> {
    let mut notes = Vec::new();
    if !matches!(state.sessions.runtimes, Load::Ready(_)) {
        notes.push("the runtimes have not been listed yet".to_owned());
    }
    if !matches!(state.accounts, Load::Ready(_)) {
        notes.push("the accounts have not been read yet".to_owned());
    }
    if !matches!(state.plugins.list, Load::Ready(_)) {
        notes.push("the plugins have not been read yet".to_owned());
    }
    notes
}

/// The runtimes a listing returned. Every one of them is live — that is what a listing is — so the
/// state says so; the app's finer states (a stop in flight, one that ended while it watched) are
/// not part of this snapshot yet.
pub(crate) fn sessions_of(state: &AppState) -> Vec<Session> {
    let Load::Ready(view) = &state.sessions.runtimes else {
        return Vec::new();
    };
    view.runtimes
        .iter()
        .map(|runtime| Session {
            // An unnamed runtime has no name to give; its account and hardware identify it.
            name: runtime.id.name.clone().unwrap_or_default(),
            account: runtime.account_email.clone().unwrap_or_default(),
            hardware: runtime.hardware.clone(),
            state: "running".to_owned(),
            auto_stop: match runtime.auto_stop_at {
                Some(_) => "scheduled".to_owned(),
                None => "none".to_owned(),
            },
            units_per_hour: runtime.estimated_rate_cu_per_hour,
        })
        .collect()
}

/// What the runtime the app last listed says it burns per hour, if the runtime is in that list.
fn live_rate(state: &AppState, name: &str) -> Option<f64> {
    let Load::Ready(view) = &state.sessions.runtimes else {
        return None;
    };
    view.runtimes
        .iter()
        .find(|runtime| runtime.id.name.as_deref() == Some(name))
        .and_then(|runtime| runtime.estimated_rate_cu_per_hour)
}

/// Each account's balance and burn rate as the app last read them. An account the app has never
/// read is an account with no numbers, never an account burning zero.
fn usage_of(state: &AppState) -> Vec<Usage> {
    let Load::Ready(view) = &state.accounts else {
        return Vec::new();
    };
    view.accounts
        .iter()
        .map(|account| Usage {
            account: account.email.clone().unwrap_or_default(),
            balance_units: account.usage.as_ref().map(|usage| usage.balance_cu),
            burn_units_per_hour: account.usage.as_ref().map(|usage| usage.rate_cu_per_hour),
            stale: account.usage.as_ref().is_some_and(|usage| usage.stale),
        })
        .collect()
}

/// The run the app is watching, if any.
pub(crate) fn runs_of(state: &AppState) -> Vec<Run> {
    state.plugin_run.iter().map(run_of).collect()
}

pub(crate) fn run_of(run: &super::runs::PluginRun) -> Run {
    let status = run.status.as_ref();
    Run {
        plugin: run.plugin.clone(),
        session: run.session.clone().unwrap_or_default(),
        // The plugin's own stage id when it published one, otherwise where the run itself is.
        stage: status
            .and_then(|status| status.stage.clone())
            .unwrap_or_else(|| phase_name(&run.phase).to_owned()),
        ready: run.live || matches!(run.phase, RunPhase::Ready(_)),
        log_tail: tail_of(run),
        workflow: status
            .map(|status| workflow_of(&status.outputs))
            .unwrap_or_default(),
        outputs: status
            .map(|status| outputs_of(&status.outputs))
            .unwrap_or_default(),
    }
}

/// An agent's `run_watch`: the run as it stands now, and what ended the wait — a new stage, ready,
/// failed, finished, or `none` when the wait's own deadline passed with nothing to report. The run
/// is the same view `run_status` answers with, so there is one shape to learn.
pub(crate) fn watch_answer(state: &AppState, plugin: &str, changed: &str) -> Result<Value> {
    let run = state
        .plugin_run()
        .filter(|run| run.plugin == plugin)
        .ok_or_else(|| Error::new(ErrorKind::PluginNotFound).with("plugin", plugin))?;
    Ok(json!({ "run": run_of(run), "changed": changed }))
}

/// The last few lines of a run's log, oldest first. Secrets are already redacted where the lines
/// were read.
fn tail_of(run: &super::runs::PluginRun) -> String {
    let lines: Vec<&str> = run.log.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

fn phase_name(phase: &RunPhase) -> &'static str {
    match phase {
        RunPhase::Running => "running",
        RunPhase::Ready(_) => "ready",
        RunPhase::Failed(_) => "failed",
        RunPhase::Cancelled => "cancelled",
    }
}

/// The name the settings file uses for an appearance. `serde` owns that name, so it is asked for
/// it here rather than spelled out a second time.
fn appearance_name(pref: &crate::settings::AppearancePref) -> String {
    serde_json::to_value(pref)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The workflow a run published, in the shape the run view draws: the output named `workflow`
/// carries `nodes` (id → node type) and `links` (pairs of ids). Read the same way the interface
/// reads it, so an agent and the window never disagree about a graph.
fn workflow_of(outputs: &serde_json::Map<String, serde_json::Value>) -> Workflow {
    let Some(workflow) = outputs.get("workflow") else {
        return Workflow::default();
    };
    let nodes = workflow
        .get("nodes")
        .and_then(|nodes| nodes.as_object())
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|(id, kind)| {
                    Some(Node {
                        id: id.clone(),
                        kind: kind.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let links = workflow
        .get("links")
        .and_then(|links| links.as_array())
        .map(|links| {
            links
                .iter()
                .filter_map(|link| {
                    let pair = link.as_array()?;
                    let (from, to) = (pair.first()?.as_str()?, pair.get(1)?.as_str()?);
                    Some(colab_playground_core::mcp::view::Link {
                        from: from.to_owned(),
                        to: to.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Workflow { nodes, links }
}

/// The files a run produced, where the plugin gave their paths on the runtime: any output value
/// that is a path of its own. This is the same rule the run view follows when it turns an output
/// into something the user can open.
fn outputs_of(outputs: &serde_json::Map<String, serde_json::Value>) -> Vec<Output> {
    let mut found = Vec::new();
    for value in outputs.values() {
        collect_paths(value, &mut found);
    }
    found
}

fn collect_paths(value: &serde_json::Value, found: &mut Vec<Output>) {
    match value {
        serde_json::Value::String(path) if is_file_path(path) => found.push(Output {
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.clone(),
        }),
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_paths(value, found);
            }
        }
        serde_json::Value::Array(items) => {
            for value in items {
                collect_paths(value, found);
            }
        }
        _ => {}
    }
}

/// Whether a string a run published names a file on the runtime: an absolute path whose last
/// segment looks like a file name. What a plugin publishes is written for people to read, so it
/// also carries directories, view URLs and plain labels, and calling those produced files sends an
/// agent after something that is not there.
fn is_file_path(path: &str) -> bool {
    !path.contains('?')
        && path.starts_with('/')
        && path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.contains('.'))
}

/// The first path a plugin's own manifest declares as worth keeping — what the run page's save
/// button copies to this computer.
pub(crate) fn keep_path(state: &AppState, plugin: &str) -> Option<String> {
    let Load::Ready(list) = &state.plugins.list else {
        return None;
    };
    list.iter()
        .find(|info| info.manifest.id == plugin)
        .and_then(|info| info.manifest.keep.first())
        .map(|keep| keep.path.clone())
}

pub(crate) fn plugins_of(state: &AppState) -> Vec<Plugin> {
    let Load::Ready(list) = &state.plugins.list else {
        return Vec::new();
    };
    list.iter()
        .map(|plugin| Plugin {
            id: plugin.manifest.id.clone(),
            // The name an agent reads is the English one: a protocol is not a place for the
            // interface's locale.
            name: plugin
                .manifest
                .name
                .get("en")
                .unwrap_or(&plugin.manifest.id)
                .to_owned(),
            enabled: plugin.enabled,
            bundled: matches!(plugin.source, PluginSource::Bundled),
        })
        .collect()
}

pub(crate) fn accounts_of(state: &AppState) -> Vec<Account> {
    let Load::Ready(view) = &state.accounts else {
        return Vec::new();
    };
    view.accounts
        .iter()
        .map(|account| Account {
            email: account.email.clone().unwrap_or_default(),
            active: view.active.as_ref() == Some(&account.id),
        })
        .collect()
}

pub(crate) fn forwards_of(state: &AppState) -> Vec<Forward> {
    state
        .sessions
        .tunnels
        .values()
        .map(|tunnel| Forward {
            session: tunnel.spec.session.clone(),
            local_port: tunnel.spec.local_port,
            remote_port: tunnel.spec.remote_port,
        })
        .collect()
}

pub(crate) fn settings_of(state: &AppState) -> AppSettings {
    let defaults = &state.settings.defaults;
    AppSettings {
        language: state.settings.language.value().to_owned(),
        appearance: appearance_name(&state.settings.appearance),
        default_hardware: format!("{:?}", defaults.accelerator),
        default_auto_stop: match defaults.max_runtime {
            Some(limit) => format!("{} min", limit.as_secs() / 60),
            None => "none".to_owned(),
        },
        wsl_distro: state.settings.wsl_distro.clone(),
        colab_path: state.settings.colab_path.clone(),
        demo_data: matches!(
            state.backend.as_ref().map(|backend| backend.kind()),
            Ok(BackendKind::Mock)
        ),
    }
}

/// What an agent is answered with once an action has landed: the state the window now shows, in
/// the shape a read tool reads it. The rule is the snapshot's — nothing here is invented, and a
/// part the app does not have (a runtime that went away between the call and its answer) is left
/// out rather than guessed.
pub(crate) fn answer_for_action(action: &Action, state: &AppState) -> Result<Value> {
    Ok(match action {
        // Answered by the window's own events: a watch is still waiting when this would be built.
        Action::RunWatch { .. } => unreachable!("a watch is answered by the window's events"),
        Action::SessionStart { .. } => session_started(state),
        Action::SessionStop { session } => json!({ "session": session, "stopped": true }),
        Action::SessionExtend { session, minutes } => json!({
            "session": session,
            "minutes": minutes,
            "auto_stop": if *minutes == 0 { "cancelled" } else { "scheduled" },
        }),
        Action::PluginRun { plugin, .. } => {
            let run = state
                .plugin_run()
                .filter(|run| &run.plugin == plugin)
                .ok_or_else(|| Error::new(ErrorKind::PluginNotFound).with("plugin", plugin))?;
            json!({ "run": run_of(run) })
        }
        Action::RunDownload { plugin } => {
            let dir = outputs_dir(plugin);
            json!({
                "plugin": plugin,
                // Which directory on the runtime it copied from: the answer is only useful if it
                // says what was looked at.
                "from": keep_path(state, plugin).unwrap_or_default(),
                "directory": dir.to_string_lossy(),
                "files": files_in(&dir),
            })
        }
        Action::SettingsSet { .. } => json!({ "settings": settings_of(state) }),
        Action::AccountsSwitch { .. } => json!({
            "accounts": accounts_of(state),
            "active": state
                .accounts()
                .ready()
                .and_then(|view| view.active_email.clone()),
        }),
        Action::PluginEnable { plugin, enabled } => json!({
            "plugin": plugin,
            "enabled": enabled,
            "plugins": plugins_of(state),
        }),
        Action::PluginInstall { .. } => json!({ "plugins": plugins_of(state) }),
        Action::ForwardOpen {
            session,
            remote_port,
            ..
        } => {
            let opened = forwards_of(state)
                .into_iter()
                .find(|forward| forward.session == *session && forward.remote_port == *remote_port);
            match opened {
                Some(forward) => json!(forward),
                // It was asked for and is not there: the runtime went away, or the forward was
                // closed again before this answer was built.
                None => {
                    return Err(Error::new(ErrorKind::PortInUse)
                        .with("remote_port", remote_port)
                        .detail("the forward is not open"));
                }
            }
        }
        Action::ForwardClose {
            session,
            local_port,
        } => json!({ "session": session, "local_port": local_port, "closed": true }),
    })
}

/// The runtime an agent's `session_start` left: what it is called, on what hardware, and what
/// that hardware burns. The figure is the published one for the hardware, which is the same one
/// the new-session form shows for a runtime that has not been measured yet.
fn session_started(state: &AppState) -> Value {
    let Some(session) = state.new_session() else {
        return json!({ "session": Value::Null });
    };
    let accelerator = session.spec.accelerator;
    let name = match &session.phase {
        ProvisionPhase::Ready(outcome) => outcome.name.clone(),
        _ => session.spec.name.clone(),
    };
    // The rate the runtime itself reports once the app has listed it — the same number
    // `sessions_list` answers with — and the published table's rate only before that.
    let rate = live_rate(state, &name)
        .unwrap_or_else(|| published_rate(accelerator, session.spec.high_mem));
    json!({
        "session": name,
        "hardware": format!("{accelerator:?}"),
        "units_per_hour": rate,
        "auto_stop_minutes": session
            .spec
            .guard
            .max_runtime
            .map(|limit| limit.as_secs() / 60),
    })
}

/// The files a download left, named as this computer has them.
pub(crate) fn files_in(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::mcp::Agents;
    use colab_playground_core::mcp::view::Snapshot;

    /// A snapshot with one runtime in it, the way a listing would have published it.
    fn snapshot() -> Snapshot {
        Snapshot {
            sessions: vec![Session {
                name: "playground-1".to_owned(),
                account: "someone@example.com".to_owned(),
                hardware: "T4".to_owned(),
                state: "running".to_owned(),
                auto_stop: "scheduled".to_owned(),
                units_per_hour: Some(1.5),
            }],
            ..Snapshot::default()
        }
    }

    /// A connection the endpoint answers over, read the way a client reads it. Enough for one
    /// response: the tests below ask one question at a time.
    fn ask(address: SocketAddr, token: &str, body: &str) -> (u16, String) {
        use std::io::{BufRead, BufReader, Read, Write};
        let stream = std::net::TcpStream::connect(address).expect("the endpoint answers");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("a read timeout");
        let mut reader = BufReader::new(stream);
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        reader
            .get_ref()
            .write_all(request.as_bytes())
            .expect("written");
        reader.get_ref().flush().expect("flushed");
        let mut line = String::new();
        reader.read_line(&mut line).expect("a status line");
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status");
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("a header");
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header
                .to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .and_then(|value| value.parse::<usize>().ok())
            {
                length = value;
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("the body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[test]
    fn nothing_listens_until_the_settings_ask_for_it() {
        let mut agents = Agents::default();
        let port = 47_611;
        agents.sync(None);
        assert!(agents.address().is_none());
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "a stopped endpoint holds no port"
        );
    }

    #[test]
    fn an_agent_reads_the_snapshot_the_app_published() {
        let mut agents = Agents::default();
        // Port 0: the system picks one, so two runs of this test cannot collide.
        agents.sync(Some((0, "a-token".to_owned())));
        let address = agents.address().expect("it listens");
        agents.publish(snapshot());
        assert_eq!(agents.token(), Some("a-token"));
        assert!(agents.error().is_none());

        let (status, body) = ask(
            address,
            "a-token",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"sessions_list"}}"#,
        );
        assert_eq!(status, 200, "{body}");
        let answer: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(answer["result"]["isError"], false, "{body}");
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        let listed: serde_json::Value = serde_json::from_str(text).expect("the tool's JSON");
        assert_eq!(listed["sessions"][0]["name"], "playground-1");
        assert_eq!(listed["sessions"][0]["hardware"], "T4");
        assert!(!text.contains("a-token"), "a tool never echoes the token");

        // The window's token is the one the endpoint demands: anything else is refused.
        let (status, _) = ask(
            address,
            "another-token",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        assert_eq!(status, 401);
    }

    #[test]
    fn turning_it_off_frees_the_port_and_rotating_the_token_restarts_it() {
        let mut agents = Agents::default();
        agents.sync(Some((0, "first".to_owned())));
        let address = agents.address().expect("it listens");

        agents.sync(Some((0, "second".to_owned())));
        assert_ne!(
            agents.token(),
            Some("first"),
            "an agent with the old token stops working"
        );
        assert!(agents.address().is_some(), "and the endpoint is back");

        agents.sync(None);
        assert!(agents.address().is_none());
        assert!(agents.error().is_none(), "stopping is not a failure");
        assert!(
            std::net::TcpListener::bind(address).is_ok(),
            "the port is free again"
        );
    }

    #[test]
    fn a_taken_port_is_reported_rather_than_panicking() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
        let port = held.local_addr().expect("an address").port();
        let mut agents = Agents::default();
        agents.sync(Some((port, "a-token".to_owned())));
        assert!(agents.address().is_none());
        let error = agents.error().expect("why it is not running");
        assert_eq!(
            error.kind,
            colab_playground_core::error::ErrorKind::PortInUse
        );
        assert_eq!(error.param("port"), Some(port.to_string().as_str()));
    }

    #[test]
    fn the_workflow_and_the_files_of_a_run_are_read_the_way_the_window_reads_them() {
        let mut outputs = serde_json::Map::new();
        outputs.insert(
            "workflow".to_owned(),
            serde_json::json!({
                "nodes": {"5": "KSampler", "6": "VAEDecode"},
                "links": [["5", "6"]],
            }),
        );
        outputs.insert(
            "image".to_owned(),
            serde_json::json!("/content/ComfyUI/output/image.png"),
        );
        let workflow = workflow_of(&outputs);
        assert_eq!(workflow.nodes.len(), 2);
        assert_eq!(workflow.nodes[0].id, "5");
        assert_eq!(workflow.nodes[0].kind, "KSampler");
        assert_eq!(workflow.links.len(), 1);
        assert_eq!(workflow.links[0].from, "5");
        assert_eq!(workflow.links[0].to, "6");
        let files = outputs_of(&outputs);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "image.png");
        assert_eq!(files[0].path, "/content/ComfyUI/output/image.png");
        // A run that published nothing readable answers with nothing, not with a guess.
        assert!(workflow_of(&serde_json::Map::new()).nodes.is_empty());
        assert!(outputs_of(&serde_json::Map::new()).is_empty());
    }
}
