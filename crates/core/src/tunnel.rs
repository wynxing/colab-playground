//! Port forwarding over `colab ssh --proxy-mode`.
//!
//! `ssh -N -L 127.0.0.1:<local>:127.0.0.1:<remote>` with the CLI as
//! `ProxyCommand` (given on the command line; `~/.ssh/config` is neither read
//! nor edited). A runtime allows a single `colab ssh` connection (HTTP 429),
//! so the tunnel owns it and everything else uses `colab exec`.
//!
//! **Cost trap:** `colab ssh --proxy-mode -s NAME` *creates* a runtime when
//! `NAME` is not in its state file. Before every (re)connect the tunnel checks
//! that the session is still recorded in the account's state file and fails
//! with `SessionGone` otherwise, so a reconnect can never allocate a new VM.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::accounts::AccountId;
use crate::cli::{AUTH_PROMPT_MARKER, RESERVED_VM_PORT, parse_ssh_session_limit};
use crate::clock::{CancelToken, Clock};
use crate::error::{Error, ErrorKind, Result};
use crate::host::{Host, ScriptRequest, shell_quote, tail};
use crate::hostfs::HostFs;
use crate::http::{local_http_status, local_port_in_use, local_port_open};
use crate::paths::HostPath;
use crate::sessions::validate_session_name;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    /// Account owning the session (must be active while the tunnel runs).
    pub account: AccountId,
    pub session: String,
    /// Local port on `127.0.0.1`.
    pub local_port: u16,
    /// Port on the VM (never 8080).
    pub remote_port: u16,
    /// Optional HTTP health path probed through the tunnel (e.g. `/health`).
    pub health_path: Option<String>,
}

impl TunnelSpec {
    pub fn validate(&self) -> Result<()> {
        validate_session_name(&self.session)?;
        if self.remote_port == RESERVED_VM_PORT {
            return Err(Error::new(ErrorKind::PortReserved).with("port", self.remote_port));
        }
        if self.remote_port == 0 || self.local_port == 0 {
            return Err(Error::invalid("port", "ports must be 1-65535"));
        }
        if let Some(p) = &self.health_path
            && (!p.starts_with('/') || p.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(Error::invalid("health_path", "must start with '/'"));
        }
        Ok(())
    }

    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TunnelId(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TunnelState {
    Starting,
    Up,
    Reconnecting {
        /// Reconnect attempt (1-based).
        attempt: u32,
        /// Delay before it in seconds.
        delay_secs: u64,
    },
    /// Gave up (`SshBusy`, `PortInUse`, `SessionGone`, `AuthRequired`, …).
    Failed {
        error: Error,
    },
    /// Closed by the user (or because its account was switched away).
    Stopped,
}

impl TunnelState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TunnelState::Failed { .. } | TunnelState::Stopped)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelInfo {
    pub id: TunnelId,
    pub spec: TunnelSpec,
    pub state: TunnelState,
    /// Last health probe result (`None` before the first probe or without a health path).
    pub healthy: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TunnelEvent {
    State {
        id: TunnelId,
        state: TunnelState,
    },
    Health {
        id: TunnelId,
        /// It answered with a status below 500.
        healthy: bool,
    },
}

/// `colab --config <state> ssh --proxy-mode -s <name>` as a shell string for
/// OpenSSH's `ProxyCommand` (which always runs through a shell).
pub fn proxy_command(colab: &str, state_file: &HostPath, session: &str) -> String {
    format!(
        "{} --config {} ssh --proxy-mode -s {}",
        shell_quote(colab),
        shell_quote(state_file.as_str()),
        shell_quote(session)
    )
}

pub fn ssh_args(colab: &str, state_file: &HostPath, spec: &TunnelSpec) -> Result<Vec<String>> {
    spec.validate()?;
    let opt = |s: &str| ["-o".to_owned(), s.to_owned()];
    let mut args = vec![
        "-N".to_owned(),
        "-T".to_owned(),
        "-F".to_owned(),
        "/dev/null".to_owned(),
    ];
    for o in [
        "ExitOnForwardFailure=yes",
        "ServerAliveInterval=30",
        "ServerAliveCountMax=3",
        "StrictHostKeyChecking=no",
        "UserKnownHostsFile=/dev/null",
        "LogLevel=ERROR",
        "BatchMode=yes",
        "ConnectTimeout=60",
    ] {
        args.extend(opt(o));
    }
    args.extend(opt(&format!(
        "ProxyCommand={}",
        proxy_command(colab, state_file, &spec.session)
    )));
    args.push("-L".into());
    args.push(format!(
        "127.0.0.1:{}:127.0.0.1:{}",
        spec.local_port, spec.remote_port
    ));
    args.push("root@colab-runtime".into());
    Ok(args)
}

/// Creates `~/.ssh/id_ed25519` only if neither it nor `id_ecdsa` has a public key.
pub const ENSURE_KEY_SCRIPT: &str = r#"[ -f "$HOME/.ssh/id_ed25519.pub" ] && exit 0
[ -f "$HOME/.ssh/id_ecdsa.pub" ] && exit 0
command -v ssh-keygen >/dev/null 2>&1 || exit 4
[ -e "$HOME/.ssh/id_ed25519" ] && exit 5
mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh" || exit 1
ssh-keygen -q -t ed25519 -N "" -C colab-playground -f "$HOME/.ssh/id_ed25519" </dev/null"#;

// Tests point `CPG_SSH` at a fake. Killing `wsl.exe` or crashing on Windows does
// not signal WSL children, which would orphan `colab ssh` and lock the runtime's
// single SSH slot (HTTP 429); the FD-3 monitor turns parent pipe EOF into SIGTERM
// (FD 3 avoids POSIX `&` redirecting FD 0 to `/dev/null` when `-m` is off).
const RUN_SSH: &str = r#"exec 3<&0 </dev/null
"${CPG_SSH:-ssh}" "$@" 3<&- &
__cpg_ssh=$!
(
  while IFS= read -r _ <&3; do :; done
  exec 3<&-
  if kill -0 "$__cpg_ssh" 2>/dev/null; then
    kill -TERM "$__cpg_ssh" 2>/dev/null
    i=0
    while kill -0 "$__cpg_ssh" 2>/dev/null && [ "$i" -lt 20 ]; do
      sleep 0.1
      i=$((i + 1))
    done
    kill -KILL "$__cpg_ssh" 2>/dev/null
  fi
) </dev/null >/dev/null 2>&1 &
__cpg_mon=$!
exec 3<&-
wait "$__cpg_ssh"
__cpg_rc=$?
kill -TERM "$__cpg_mon" 2>/dev/null
exit "$__cpg_rc""#;

/// Maps ssh / ProxyCommand output to a failure kind (`None` = transient).
pub fn classify_ssh_output(stderr: &str, code: Option<i32>) -> Option<ErrorKind> {
    let has = |s: &str| stderr.contains(s);
    if has("(HTTP 429)") {
        Some(ErrorKind::SshBusy)
    } else if has("Address already in use")
        || has("cannot listen to port")
        || has("Could not request local forwarding")
    {
        Some(ErrorKind::PortInUse)
    } else if has(AUTH_PROMPT_MARKER) {
        Some(ErrorKind::AuthRequired)
    } else if has("Could not read the OAuth token from the OS keyring") {
        Some(ErrorKind::KeyringUnavailable)
    } else if (has("ession '") && has("not found"))
        || has("appears to be lost")
        || has("(HTTP 404)")
    {
        Some(ErrorKind::SessionGone)
    } else if code == Some(127) {
        Some(ErrorKind::NotInstalled)
    } else {
        None
    }
}

/// Reconnect delay: 1, 2, 4, 8, 16, 30, 30… seconds.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5)).min(Duration::from_secs(30))
}

/// True when the CLI state file (JSON object keyed by session name) records `session`.
pub fn state_has_session(state_json: &str, session: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(state_json)
        .map(|m| m.contains_key(session))
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct TunnelCtx {
    pub host: Host,
    pub colab: String,
    pub base_env: Vec<(String, String)>,
    /// The owning account's state file.
    pub state_file: HostPath,
    pub clock: Arc<dyn Clock>,
}

const BUSY_LIMIT: u32 = 5;
const QUICK_FAILURE_LIMIT: u32 = 8;
const UP_TIMEOUT: Duration = Duration::from_secs(120);
const HEALTH_EVERY: Duration = Duration::from_secs(15);

struct Shared {
    info: Mutex<TunnelInfo>,
    events: Option<Sender<TunnelEvent>>,
}

impl Shared {
    fn set(&self, state: TunnelState) {
        let id = {
            let mut i = self.info.lock().unwrap_or_else(|e| e.into_inner());
            if i.state == state {
                return;
            }
            i.state = state.clone();
            i.id
        };
        if let Some(tx) = &self.events {
            let _ = tx.send(TunnelEvent::State { id, state });
        }
    }

    fn health(&self, healthy: bool) {
        let id = {
            let mut i = self.info.lock().unwrap_or_else(|e| e.into_inner());
            i.healthy = Some(healthy);
            i.id
        };
        if let Some(tx) = &self.events {
            let _ = tx.send(TunnelEvent::Health { id, healthy });
        }
    }
}

fn fail(shared: &Shared, error: Error) {
    shared.set(TunnelState::Failed { error });
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RetryCounters {
    attempt: u32,
    busy: u32,
    quick: u32,
}

fn decide_after_exit(
    counters: &mut RetryCounters,
    spec: &TunnelSpec,
    text: &str,
    code: Option<i32>,
    uptime: Duration,
    gave_up: Option<Error>,
) -> Result<Duration> {
    match classify_ssh_output(text, code) {
        Some(ErrorKind::SshBusy) => {
            counters.busy += 1;
            if counters.busy >= BUSY_LIMIT {
                let mut err = Error::new(ErrorKind::SshBusy).detail(tail(text, 400));
                if let Some(limit) = parse_ssh_session_limit(text) {
                    err = err.with("limit", limit);
                }
                return Err(err);
            }
        }
        Some(ErrorKind::PortInUse) => {
            return Err(Error::new(ErrorKind::PortInUse)
                .with("port", spec.local_port)
                .detail(tail(text, 400)));
        }
        Some(kind) => {
            return Err(Error::new(kind)
                .with("session", &spec.session)
                .detail(tail(text, 400)));
        }
        None => {
            // `UP_TIMEOUT` exceeds the 10s quick-failure window, so check `gave_up` before resetting `quick`.
            if let Some(e) = gave_up {
                let stderr_tail = tail(text, 400);
                let detail = match (e.detail.is_empty(), stderr_tail.is_empty()) {
                    (false, false) => format!("{}: {stderr_tail}", e.detail),
                    (false, true) => e.detail.clone(),
                    (true, _) => stderr_tail,
                };
                return Err(e.detail(detail));
            }
            counters.busy = 0;
            if uptime < Duration::from_secs(10) {
                counters.quick += 1;
            } else {
                counters.quick = 0;
                counters.attempt = 0;
            }
            if counters.quick >= QUICK_FAILURE_LIMIT {
                return Err(Error::new(ErrorKind::Other).detail(tail(text, 400)));
            }
        }
    }
    counters.attempt += 1;
    Ok(backoff(counters.attempt))
}

fn run(ctx: TunnelCtx, spec: TunnelSpec, stop: CancelToken, shared: Arc<Shared>) {
    let fs = HostFs::new(&ctx.host, &ctx.base_env);
    let key = ScriptRequest {
        env: ctx.base_env.clone(),
        ..ScriptRequest::new(ENSURE_KEY_SCRIPT)
    };
    match ctx
        .host
        .run_quiet(key.timeout(Duration::from_secs(60)), &stop)
    {
        Ok(out) if out.success() => {}
        Ok(out) if out.code == Some(4) => {
            return fail(
                &shared,
                Error::new(ErrorKind::NotInstalled).with("program", "ssh-keygen"),
            );
        }
        Ok(out) => {
            return fail(
                &shared,
                Error::new(ErrorKind::Other)
                    .detail(format!("ssh key setup: {}", tail(&out.stderr, 300))),
            );
        }
        Err(e) if e.kind == ErrorKind::Cancelled => return shared.set(TunnelState::Stopped),
        Err(e) => return fail(&shared, e),
    }
    let args = match ssh_args(&ctx.colab, &ctx.state_file, &spec) {
        Ok(a) => a,
        Err(e) => return fail(&shared, e),
    };
    let mut counters = RetryCounters::default();
    loop {
        if stop.is_cancelled() {
            return shared.set(TunnelState::Stopped);
        }
        // Never let `--proxy-mode` auto-create a runtime: the session must be recorded.
        match fs.read(&ctx.state_file, &stop) {
            Ok(Some(text)) if state_has_session(&text, &spec.session) => {}
            Ok(_) => {
                return fail(
                    &shared,
                    Error::new(ErrorKind::SessionGone).with("session", &spec.session),
                );
            }
            Err(e) if e.kind == ErrorKind::Cancelled => return shared.set(TunnelState::Stopped),
            Err(_) => {
                counters.attempt += 1;
                counters.quick += 1;
                if counters.quick >= QUICK_FAILURE_LIMIT {
                    return fail(
                        &shared,
                        Error::new(ErrorKind::Other).detail("cannot read the session state file"),
                    );
                }
                let delay = backoff(counters.attempt);
                shared.set(TunnelState::Reconnecting {
                    attempt: counters.attempt,
                    delay_secs: delay.as_secs(),
                });
                if ctx.clock.sleep(delay, &stop).is_err() {
                    return shared.set(TunnelState::Stopped);
                }
                continue;
            }
        }
        if local_port_in_use(spec.local_port, Duration::from_millis(300)) {
            return fail(
                &shared,
                Error::new(ErrorKind::PortInUse).with("port", spec.local_port),
            );
        }
        if counters.attempt == 0 {
            shared.set(TunnelState::Starting);
        }
        let mut req = ScriptRequest::new(RUN_SSH).args(args.clone()).stdin_pipe();
        req.env = ctx.base_env.clone();
        let mut child = match ctx.host.spawn(req) {
            Ok(c) => c,
            Err(e) => return fail(&shared, e),
        };
        let started = Instant::now();
        let mut up = false;
        let mut last_health: Option<Instant> = None;
        let mut stderr = Vec::new();
        let mut gave_up: Option<Error> = None;
        loop {
            if stop.is_cancelled() {
                child.close_stdin();
                child.kill();
                return shared.set(TunnelState::Stopped);
            }
            match child.recv_line(Duration::from_millis(250)) {
                Ok(line) => {
                    if line.text.contains(AUTH_PROMPT_MARKER) {
                        child.close_stdin();
                        child.kill();
                        return fail(&shared, Error::new(ErrorKind::AuthRequired));
                    }
                    stderr.push(line.text);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if child.try_wait().is_some() {
                while let Ok(line) = child.recv_line(Duration::from_millis(100)) {
                    stderr.push(line.text);
                }
                break;
            }
            if !up && local_port_open(spec.local_port, Duration::from_millis(300)) {
                up = true;
                shared.set(TunnelState::Up);
            }
            if up {
                if let Some(path) = &spec.health_path
                    && last_health.is_none_or(|t| t.elapsed() >= HEALTH_EVERY)
                {
                    last_health = Some(Instant::now());
                    let ok = local_http_status(spec.local_port, path, Duration::from_secs(10))
                        .is_ok_and(|s| s < 500);
                    shared.health(ok);
                }
            } else if started.elapsed() > UP_TIMEOUT {
                child.close_stdin();
                child.kill();
                gave_up =
                    Some(Error::new(ErrorKind::Timeout).detail("the forward did not come up"));
                break;
            }
        }
        child.close_stdin();
        let code = child.wait();
        let text = stderr.join("\n");
        let uptime = started.elapsed();
        let delay = match decide_after_exit(&mut counters, &spec, &text, code, uptime, gave_up) {
            Ok(d) => d,
            Err(e) => return fail(&shared, e),
        };
        shared.set(TunnelState::Reconnecting {
            attempt: counters.attempt,
            delay_secs: delay.as_secs(),
        });
        if ctx.clock.sleep(delay, &stop).is_err() {
            return shared.set(TunnelState::Stopped);
        }
    }
}

struct Entry {
    shared: Arc<Shared>,
    stop: CancelToken,
    thread: Option<JoinHandle<()>>,
}

/// Owns tunnel threads. Dropping it closes every tunnel.
#[derive(Default)]
pub struct TunnelManager {
    next: AtomicU64,
    tunnels: Mutex<BTreeMap<TunnelId, Entry>>,
}

impl std::fmt::Debug for TunnelManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelManager")
            .field("tunnels", &self.list().len())
            .finish()
    }
}

impl TunnelManager {
    pub fn new() -> Self {
        TunnelManager::default()
    }

    pub fn open(
        &self,
        ctx: TunnelCtx,
        spec: TunnelSpec,
        events: Option<Sender<TunnelEvent>>,
    ) -> Result<TunnelId> {
        spec.validate()?;
        let mut map = self.tunnels.lock().unwrap_or_else(|e| e.into_inner());
        let clash = map.values().any(|e| {
            let i = e.shared.info.lock().unwrap_or_else(|e| e.into_inner());
            !i.state.is_terminal() && i.spec.local_port == spec.local_port
        });
        if clash {
            return Err(Error::new(ErrorKind::PortInUse).with("port", spec.local_port));
        }
        let id = TunnelId(self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let shared = Arc::new(Shared {
            info: Mutex::new(TunnelInfo {
                id,
                spec: spec.clone(),
                state: TunnelState::Starting,
                healthy: None,
            }),
            events,
        });
        let stop = CancelToken::new();
        let (s2, stop2) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("tunnel-{}", id.0))
            .spawn(move || run(ctx, spec, stop2, s2))
            .map_err(Error::from)?;
        map.insert(
            id,
            Entry {
                shared,
                stop,
                thread: Some(thread),
            },
        );
        Ok(id)
    }

    /// Closing does not stop billing.
    pub fn close(&self, id: TunnelId) -> Result<()> {
        let entry = self
            .tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        let Some(mut entry) = entry else {
            return Err(Error::invalid("tunnel", "unknown tunnel"));
        };
        entry.stop.cancel();
        if let Some(t) = entry.thread.take() {
            let _ = t.join();
        }
        entry.shared.set(TunnelState::Stopped);
        Ok(())
    }

    pub fn close_session(&self, account: &AccountId, session: &str) {
        for t in self
            .list()
            .into_iter()
            .filter(|t| &t.spec.account == account && t.spec.session == session)
        {
            let _ = self.close(t.id);
        }
    }

    /// Snapshots of all tunnels (including failed ones until closed).
    pub fn list(&self) -> Vec<TunnelInfo> {
        self.tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|e| {
                e.shared
                    .info
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .collect()
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        let ids: Vec<TunnelId> = self
            .tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        for id in ids {
            let _ = self.close(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TunnelSpec {
        TunnelSpec {
            account: AccountId::new("a1").unwrap(),
            session: "llm".into(),
            local_port: 18000,
            remote_port: 8000,
            health_path: Some("/health".into()),
        }
    }

    #[test]
    fn ssh_argv_uses_per_account_config_in_proxy_command() {
        let state =
            HostPath::new("/home/u/.local/share/colab-playground/accounts/a1/sessions.json");
        let args = ssh_args("/home/u/.local/bin/colab", &state, &spec()).unwrap();
        assert_eq!(&args[..4], ["-N", "-T", "-F", "/dev/null"]);
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_owned()));
        assert!(args.contains(&"ProxyCommand=/home/u/.local/bin/colab --config /home/u/.local/share/colab-playground/accounts/a1/sessions.json ssh --proxy-mode -s llm".to_owned()));
        let l = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[l + 1], "127.0.0.1:18000:127.0.0.1:8000");
        assert_eq!(args.last().unwrap(), "root@colab-runtime");
    }

    #[test]
    fn proxy_command_quotes_odd_paths() {
        let p = proxy_command(
            "/opt/my tools/colab",
            &HostPath::new("/d/it's/s.json"),
            "s1",
        );
        assert_eq!(
            p,
            r"'/opt/my tools/colab' --config '/d/it'\''s/s.json' ssh --proxy-mode -s s1"
        );
    }

    #[test]
    fn port_8080_and_bad_input_are_rejected() {
        let mut s = spec();
        s.remote_port = 8080;
        assert!(s.validate().unwrap_err().is(ErrorKind::PortReserved));
        s.remote_port = 8000;
        s.session = "a b".into();
        assert!(s.validate().is_err());
        s.session = "ok".into();
        s.health_path = Some("health".into());
        assert!(s.validate().is_err());
    }

    #[test]
    fn classification_and_backoff() {
        let c = |s: &str| classify_ssh_output(s, Some(255));
        assert_eq!(
            c("[colab] Already-active SSH session (HTTP 429): another"),
            Some(ErrorKind::SshBusy)
        );
        assert_eq!(
            c("[colab] SSH session limit reached (HTTP 429): 5 of 5 SSH sessions in use on this runtime."),
            Some(ErrorKind::SshBusy)
        );
        assert_eq!(
            c(
                "bind [127.0.0.1]:8000: Address already in use\nchannel_setup_fwd_listener_tcpip: cannot listen to port: 8000"
            ),
            Some(ErrorKind::PortInUse)
        );
        assert_eq!(
            c("[colab] session 'llm' not found. Run `colab sessions`"),
            Some(ErrorKind::SessionGone)
        );
        assert_eq!(
            c("kex_exchange_identification: Connection closed by remote host"),
            None
        );
        assert_eq!(
            classify_ssh_output("bash: ssh: command not found", Some(127)),
            Some(ErrorKind::NotInstalled)
        );
        let secs: Vec<u64> = (1..=8).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn ssh_busy_carries_session_limit_when_reported() {
        let s = spec();
        let mut c = RetryCounters::default();
        for _ in 0..(BUSY_LIMIT - 1) {
            assert!(
                decide_after_exit(
                    &mut c,
                    &s,
                    "[colab] SSH session limit reached (HTTP 429): 5 of 5 SSH sessions in use on this runtime.",
                    Some(255),
                    Duration::from_secs(1),
                    None,
                )
                .is_ok()
            );
        }
        let err = decide_after_exit(
            &mut c,
            &s,
            "[colab] SSH session limit reached (HTTP 429): 5 of 5 SSH sessions in use on this runtime.",
            Some(255),
            Duration::from_secs(1),
            None,
        )
        .unwrap_err();
        assert!(err.is(ErrorKind::SshBusy));
        assert_eq!(err.param("limit"), Some("5"));
    }

    #[test]
    fn state_file_check() {
        assert!(state_has_session(r#"{"llm": {"endpoint": "e"}}"#, "llm"));
        assert!(!state_has_session(r#"{"other": {}}"#, "llm"));
        assert!(!state_has_session("", "llm"));
        assert!(!state_has_session("not json", "llm"));
    }

    #[test]
    fn startup_timeout_fails_immediately_without_infinite_reconnect_loop() {
        let s = spec();
        let mut c = RetryCounters::default();
        let err = decide_after_exit(
            &mut c,
            &s,
            "",
            None,
            Duration::from_secs(121),
            Some(Error::new(ErrorKind::Timeout).detail("the forward did not come up")),
        )
        .unwrap_err();
        assert!(err.is(ErrorKind::Timeout));
        assert_eq!(err.detail, "the forward did not come up");

        let mut c2 = RetryCounters::default();
        let err2 = decide_after_exit(
            &mut c2,
            &s,
            "proxy hung on handshake",
            Some(255),
            Duration::from_secs(121),
            Some(Error::new(ErrorKind::Timeout).detail("the forward did not come up")),
        )
        .unwrap_err();
        assert!(err2.is(ErrorKind::Timeout));
        assert_eq!(
            err2.detail,
            "the forward did not come up: proxy hung on handshake"
        );
    }

    #[test]
    fn exit_decisions_reset_after_long_uptime_and_bound_quick_failures() {
        let s = spec();
        let mut c = RetryCounters::default();
        for i in 1..QUICK_FAILURE_LIMIT {
            let d = decide_after_exit(
                &mut c,
                &s,
                "closed",
                Some(255),
                Duration::from_secs(1),
                None,
            )
            .unwrap();
            assert_eq!(c.quick, i);
            assert_eq!(d, backoff(i));
        }
        let d = decide_after_exit(
            &mut c,
            &s,
            "networkblip",
            Some(255),
            Duration::from_secs(30),
            None,
        )
        .unwrap();
        assert_eq!(c.quick, 0);
        assert_eq!(c.attempt, 1);
        assert_eq!(d, Duration::from_secs(1));

        for _ in 1..QUICK_FAILURE_LIMIT {
            decide_after_exit(
                &mut c,
                &s,
                "closed",
                Some(255),
                Duration::from_secs(1),
                None,
            )
            .unwrap();
        }
        let err = decide_after_exit(&mut c, &s, "boom", Some(255), Duration::from_secs(1), None)
            .unwrap_err();
        assert!(err.is(ErrorKind::Other));
        assert_eq!(err.detail, "boom");
    }

    #[cfg(unix)]
    #[test]
    fn stdin_eof_lifeline_terminates_ssh_when_parent_pipe_closes() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let fake_ssh = dir.path().join("fake-ssh");
        let marker = dir.path().join("alive");
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\ntrap 'rm -f \"$1\"; exit 0' TERM\necho $$ > \"$1\"\nwhile :; do sleep 0.1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let req = ScriptRequest::new(RUN_SSH)
            .arg(marker.to_string_lossy().into_owned())
            .env("CPG_SSH", fake_ssh.to_string_lossy().into_owned())
            .stdin_pipe();
        let mut child = Host::Local.spawn(req).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(marker.exists(), "fake ssh never started");

        // Closing stdin without `kill()` simulates an abrupt parent or `wsl.exe` exit.
        child.close_stdin();
        let wait_deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().is_none() && Instant::now() < wait_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            child.try_wait().is_some(),
            "RUN_SSH did not exit on stdin EOF"
        );
        assert!(
            !marker.exists(),
            "fake ssh did not receive SIGTERM on stdin EOF"
        );
    }
}
