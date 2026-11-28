//! Native Colab client: Public API v1beta, tunnel fallback, runtime proxy.
//!
//! Tokens stay in the vault ([`vault::TokenVault`]) or in memory
//! ([`protocol::ConnectionInfo`]). Ledger and account files store resource names
//! and endpoints only. This backend never spawns `colab` or `wsl.exe`.
//!
//! [`Backend::kind`] reports [`BackendKind::Real`] so the demo badge stays off.
//! The app chooses this backend or the CLI backend; the CLI backend remains
//! the default.

mod api;
mod protocol;
mod transport;
mod vault;

#[cfg(test)]
mod mock_colab;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};

use crate::accounts::{Account, AccountId, AccountStore, AccountsView, validate_secret_name};
use crate::backend::{
    self, AccountUsageView, Backend, BackendKind, DriveMountStart, Events, Extension,
    GuardDecision, GuardReport, HardwareCatalog, InstallMethod, LoginFlow, PluginRunEvent,
    PluginRunOutcome, PluginRunSpec, RemoteSsh, Runtime, RuntimeRef, RuntimesView, StopOptions,
    SwitchImpact, VmEntry, VmFile,
};
use crate::clock::{CancelToken, Clock, SystemClock};
use crate::defaults::Defaults;
use crate::error::{Error, ErrorKind, Result};
use crate::guard::{self, GuardPolicy, SpendMeter};
use crate::hf::HfModel;
use crate::host::{Line, ProbeReport};
use crate::keep::{KeepPlan, KeepSyncReport, KeepTargetId};
use crate::paths::write_atomic;
use crate::plugins::{PluginInfo, PluginPreview};
use crate::provision::{
    self, Hardware, ProvisionEvent, ProvisionOps, ProvisionOutcome, ProvisionSpec,
};
use crate::quant::GgufListing;
use crate::sessions::{
    self, Accelerator, Ledger, ServerSession, StopReason, TrackedSession, WatchdogInfo,
};
use crate::tunnel::{TunnelEvent, TunnelId, TunnelInfo, TunnelSpec, TunnelState};
use crate::usage::{self, AccountUsage, published_rate};

use api::Api;
use protocol::{ConnectionInfo, Endpoints, ListedRuntime};
use transport::{AutoHttp, HttpExchange, StdWs, WsConn, WsDial};
use vault::{SecretToolVault, TokenVault, account_item, secret_item};

const PROBE_CODE: &str = "\
import subprocess
out = subprocess.check_output(['nvidia-smi','--query-gpu=memory.total,name','--format=csv,noheader,nounits'], text=True)
mem, name = [part.strip() for part in out.split(',', 1)]
print('CPG_VRAM ' + str(int(float(mem))))
print('CPG_GPU ' + name)
";

/// OAuth client the user supplies. The secret is never printed.
#[derive(Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

pub struct DirectConfig {
    pub clock: Arc<dyn Clock>,
    pub http: Arc<dyn HttpExchange>,
    pub ws: Arc<dyn WsDial>,
    pub vault: Arc<dyn TokenVault>,
    pub data_dir: Option<PathBuf>,
    pub endpoints: Endpoints,
    pub oauth: Option<OAuthClient>,
    pub retry_delays: Vec<Duration>,
    pub upload_chunk_bytes: usize,
    pub ssh_pubkey: Option<String>,
    /// Private key OpenSSH uses for `-L`. `None` lets `ssh` pick its default identity.
    pub ssh_identity: Option<PathBuf>,
    /// Login on the runtime sshd. `None` is `root`, which Colab expects.
    pub ssh_user: Option<String>,
    pub adc_path: Option<PathBuf>,
    pub defaults: Defaults,
}

impl std::fmt::Debug for DirectConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectConfig")
            .field("data_dir", &self.data_dir)
            .field("adc_path", &self.adc_path)
            .field("oauth", &self.oauth.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

struct Access {
    token: String,
    expires_at: SystemTime,
}

struct LiveSession {
    resource: String,
    endpoint: String,
    accelerator: Accelerator,
    high_mem: bool,
    connection: Option<ConnectionInfo>,
}

/// Resource name and endpoint from a listing. No proxy token.
struct ListedSlot {
    resource: String,
    endpoint: String,
}

struct LiveTunnel {
    spec: TunnelSpec,
    state: TunnelState,
    stop: Arc<AtomicBool>,
    ssh: Option<SshProcess>,
}

struct SshProcess(Option<std::process::Child>);

impl Drop for SshProcess {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct State {
    defaults: Defaults,
    accounts: AccountStore,
    ledger: Ledger,
    active: Option<AccountId>,
    usage: BTreeMap<AccountId, AccountUsage>,
    access: BTreeMap<AccountId, Access>,
    sessions: BTreeMap<(AccountId, String), LiveSession>,
    /// Every runtime last listed, keyed by account and endpoint, including ones with no ledger row.
    listed: BTreeMap<(AccountId, String), ListedSlot>,
    tunnels: BTreeMap<u64, LiveTunnel>,
    next_tunnel: u64,
}

/// Colab over HTTPS and the runtime proxy, with the CLI left as a separate backend.
pub struct DirectBackend {
    api: Api,
    ws: Arc<dyn WsDial>,
    vault: Arc<dyn TokenVault>,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
    oauth: Arc<Mutex<Option<OAuthClient>>>,
    upload_chunk_bytes: usize,
    ssh_pubkey: Mutex<Option<String>>,
    ssh_identity: Option<PathBuf>,
    ssh_user: String,
    adc_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
}

impl std::fmt::Debug for DirectBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DirectBackend")
    }
}

impl DirectBackend {
    pub fn open(config: DirectConfig) -> Result<Self> {
        if let Some(dir) = &config.data_dir {
            fs::create_dir_all(dir)?;
        }
        let accounts = match &config.data_dir {
            Some(dir) => AccountStore::load(&dir.join("accounts.json"))?,
            None => AccountStore::in_memory(),
        };
        let ledger = match &config.data_dir {
            Some(dir) => Ledger::load(&dir.join("ledger.json"))?,
            None => Ledger::in_memory(),
        };
        let active = config
            .data_dir
            .as_ref()
            .and_then(|dir| read_active(&dir.join("direct.json")));
        let calls = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let api = Api::new(
            config.http,
            config.clock.clone(),
            config.endpoints,
            config.retry_delays,
            calls,
        );
        Ok(DirectBackend {
            api,
            ws: config.ws,
            vault: config.vault,
            clock: config.clock,
            state: Arc::new(Mutex::new(State {
                defaults: config.defaults,
                accounts,
                ledger,
                active,
                usage: BTreeMap::new(),
                access: BTreeMap::new(),
                sessions: BTreeMap::new(),
                listed: BTreeMap::new(),
                tunnels: BTreeMap::new(),
                next_tunnel: 1,
            })),
            oauth: Arc::new(Mutex::new(config.oauth)),
            upload_chunk_bytes: config.upload_chunk_bytes.max(1),
            ssh_pubkey: Mutex::new(config.ssh_pubkey),
            ssh_identity: config.ssh_identity,
            ssh_user: config.ssh_user.unwrap_or_else(|| "root".to_owned()),
            adc_path: config.adc_path,
            data_dir: config.data_dir,
        })
    }

    /// App constructor: CLI stays the default until settings select this backend.
    /// OAuth client JSON is read from `oauth_path` or `{data}/direct/oauth_client.json`.
    /// The file is not logged. A missing file only fails at login.
    pub fn production(
        defaults: Defaults,
        oauth_path: Option<PathBuf>,
        adc_path: Option<PathBuf>,
    ) -> Result<Self> {
        let root = crate::paths::default_local_data_dir()
            .ok_or_else(|| Error::new(ErrorKind::Io).detail("no data directory"))?;
        let data_dir = root.join("direct");
        fs::create_dir_all(&data_dir)?;
        let oauth_path = oauth_path.or_else(|| {
            let path = data_dir.join("oauth_client.json");
            path.is_file().then_some(path)
        });
        let oauth = oauth_path.map(|path| read_oauth(&path)).transpose()?;
        Self::open(DirectConfig {
            clock: Arc::new(SystemClock),
            http: Arc::new(AutoHttp),
            ws: Arc::new(StdWs::default()),
            vault: Arc::new(SecretToolVault::new()),
            data_dir: Some(data_dir),
            endpoints: Endpoints::google(),
            oauth,
            retry_delays: vec![
                Duration::from_secs(10),
                Duration::from_secs(20),
                Duration::from_secs(40),
            ],
            upload_chunk_bytes: 1024 * 1024,
            ssh_pubkey: None,
            ssh_identity: None,
            ssh_user: None,
            adc_path,
            defaults,
        })
    }

    pub fn recent_calls(&self) -> Vec<String> {
        self.api.recent_calls()
    }

    /// Runs code on a runtime through the Jupyter kernel websocket.
    /// The proxy token is a header, never part of `code`.
    pub fn execute_code(
        &self,
        target: &RuntimeRef,
        code: &str,
        cancel: &CancelToken,
    ) -> Result<String> {
        cancel.check()?;
        let connection = self.connection_of(target, cancel)?;
        let ws_url = ws_url(
            &connection.url,
            &format!(
                "/api/kernels/cpg/channels?colab-runtime-proxy-token={}",
                protocol::encode(&connection.token)
            ),
        )?;
        let mut socket = self.ws.open(
            &ws_url,
            &[
                (
                    "X-Colab-Runtime-Proxy-Token".into(),
                    connection.token.clone(),
                ),
                ("X-Colab-Client-Agent".into(), protocol::CLIENT_AGENT.into()),
            ],
            Duration::from_secs(15),
        )?;
        self.api.record("GET", "/api/kernels/cpg/channels", 101);
        let message = json!({
            "header": {
                "msg_id": api::runtime_suffix(),
                "msg_type": "execute_request",
                "session": "cpg",
                "username": "colab",
                "version": "5.3"
            },
            "parent_header": {},
            "metadata": {},
            "content": {
                "code": code,
                "silent": false,
                "store_history": false,
                "user_expressions": {},
                "allow_stdin": false,
                "stop_on_error": true
            },
            "channel": "shell"
        });
        socket.send(0x1, message.to_string().as_bytes())?;
        let mut stdout = String::new();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            cancel.check()?;
            if Instant::now() > deadline {
                return Err(Error::new(ErrorKind::ExecTimeout));
            }
            match socket.recv(Duration::from_millis(500)) {
                Ok((0x8, _)) => break,
                Ok((0x9, payload)) => socket.send(0xA, &payload)?,
                Ok((0x1, payload)) => {
                    let text = String::from_utf8_lossy(&payload);
                    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let kind = value
                        .pointer("/header/msg_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if kind == "stream" {
                        if let Some(chunk) = value.pointer("/content/text").and_then(|v| v.as_str())
                        {
                            stdout.push_str(chunk);
                        }
                    } else if kind == "execute_reply" {
                        let status = value
                            .pointer("/content/status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("ok");
                        if status != "ok" {
                            return Err(
                                Error::new(ErrorKind::Other).detail("kernel status was not ok")
                            );
                        }
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) if is_timeout(&error) => {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(stdout)
    }

    fn with_state<T>(&self, body: impl FnOnce(&mut State) -> T) -> T {
        let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        body(&mut guard)
    }

    fn oauth(&self) -> Result<OAuthClient> {
        let guard = self.oauth.lock().unwrap_or_else(|error| error.into_inner());
        guard
            .clone()
            .ok_or_else(|| Error::invalid("oauth", "oauth client config is missing"))
    }

    fn bearer(&self, id: &AccountId, cancel: &CancelToken) -> Result<String> {
        self.known_account(id)?;
        let cached = self.with_state(|state| {
            state.access.get(id).and_then(|access| {
                (access.expires_at > self.clock.now() + Duration::from_secs(30))
                    .then(|| access.token.clone())
            })
        });
        if let Some(token) = cached {
            return Ok(token);
        }
        let stored = self
            .vault
            .lookup(&account_item(id))?
            .ok_or_else(|| Error::new(ErrorKind::AuthRequired))?;
        let refresh = refresh_from_stored(&stored)?;
        let oauth = self.oauth()?;
        let bundle = self
            .api
            .refresh(&oauth.client_id, &oauth.client_secret, &refresh, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        if !bundle.refresh_token.is_empty() {
            self.vault.store(
                &account_item(id),
                &json!({ "refresh_token": bundle.refresh_token }).to_string(),
            )?;
        }
        self.remember_access(id, &bundle.access_token, bundle.expires_in);
        Ok(bundle.access_token)
    }

    fn remember_access(&self, id: &AccountId, token: &str, expires_in: u64) {
        let expires_at = self.clock.now() + Duration::from_secs(expires_in.max(60));
        self.with_state(|state| {
            state.access.insert(
                id.clone(),
                Access {
                    token: token.to_owned(),
                    expires_at,
                },
            );
        });
    }

    fn known_account(&self, id: &AccountId) -> Result<()> {
        let found = self.with_state(|state| state.accounts.get(id).is_some());
        if found {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::AccountNotFound).with("account", id.as_str()))
        }
    }

    fn require_active(&self) -> Result<AccountId> {
        self.with_state(|state| state.active.clone())
            .ok_or_else(|| Error::new(ErrorKind::NoActiveAccount))
    }

    fn save_active(&self) -> Result<()> {
        let Some(dir) = &self.data_dir else {
            return Ok(());
        };
        let active =
            self.with_state(|state| state.active.as_ref().map(|id| id.as_str().to_owned()));
        let bytes = serde_json::to_vec_pretty(&json!({ "active": active }))?;
        write_atomic(&dir.join("direct.json"), &bytes)?;
        Ok(())
    }

    fn accounts_view(&self) -> Result<AccountsView> {
        let unadopted = self.adc_unadopted()?;
        self.with_state(|state| {
            let active_email = state
                .active
                .as_ref()
                .and_then(|id| state.accounts.get(id))
                .and_then(|account| account.email.clone());
            let has_token = state
                .active
                .as_ref()
                .is_some_and(|id| state.access.contains_key(id))
                || unadopted;
            Ok(AccountsView {
                accounts: state.accounts.all().to_vec(),
                active: state.active.clone(),
                has_token,
                unadopted,
                active_email,
                recovered_pending_add: false,
            })
        })
    }

    fn adc_unadopted(&self) -> Result<bool> {
        let Some(path) = &self.adc_path else {
            return Ok(false);
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let has_refresh = value
            .get("refresh_token")
            .and_then(|token| token.as_str())
            .is_some_and(|token| !token.is_empty());
        if !has_refresh {
            return Ok(false);
        }
        let ids = self.with_state(|state| state.accounts.ids());
        for id in &ids {
            if self.vault.lookup(&account_item(id))?.is_some() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn connection_of(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<ConnectionInfo> {
        self.known_account(&target.account)?;
        let _ = self.refresh_account(&target.account, cancel);
        let found = self.with_state(|state| {
            session_for(state, target).and_then(|session| session.connection.clone())
        });
        found.ok_or_else(|| {
            Error::new(ErrorKind::SessionGone).detail("runtime has no proxy connection")
        })
    }

    fn refresh_account(
        &self,
        account: &AccountId,
        cancel: &CancelToken,
    ) -> Result<Vec<ListedRuntime>> {
        let bearer = self.bearer(account, cancel)?;
        let listed = self
            .api
            .list_runtimes(&bearer, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        self.with_state(|state| {
            for runtime in &listed {
                if runtime.listed.endpoint.is_empty() || runtime.listed.resource.is_empty() {
                    continue;
                }
                state.listed.insert(
                    (account.clone(), runtime.listed.endpoint.clone()),
                    ListedSlot {
                        resource: runtime.listed.resource.clone(),
                        endpoint: runtime.listed.endpoint.clone(),
                    },
                );
                if let Some(connection) = &runtime.connection {
                    attach_connection(state, account, runtime, connection);
                }
            }
        });
        Ok(listed.into_iter().map(|runtime| runtime.listed).collect())
    }

    fn runtime_rows(&self, cancel: &CancelToken) -> Result<RuntimesView> {
        let (accounts, active) = self.with_state(|state| {
            let mut ids = state.accounts.ids();
            if let Some(active) = &state.active {
                ids.sort_by_key(|id| id != active);
            }
            (ids, state.active.clone())
        });
        let mut view = RuntimesView::default();
        for account in accounts {
            match self.rows_for(&account, active.as_ref(), cancel) {
                Ok(rows) => view.runtimes.extend(rows),
                Err(error) => view.errors.push(backend::AccountError { account, error }),
            }
        }
        Ok(view)
    }

    fn rows_for(
        &self,
        account: &AccountId,
        active: Option<&AccountId>,
        cancel: &CancelToken,
    ) -> Result<Vec<Runtime>> {
        let email = self.with_state(|state| {
            state
                .accounts
                .get(account)
                .and_then(|item| item.email.clone())
        });
        let listed = self.refresh_account(account, cancel)?;
        self.with_state(|state| {
            let tunnels = |name: &str| {
                state
                    .tunnels
                    .iter()
                    .filter(|(_, tunnel)| {
                        &tunnel.spec.account == account && tunnel.spec.session == name
                    })
                    .map(|(id, _)| TunnelId(*id))
                    .collect::<Vec<_>>()
            };
            let mut rows = Vec::new();
            let mut seen = BTreeSet::new();
            for runtime in &listed {
                let tracked = state
                    .ledger
                    .live_by_endpoint(account, &runtime.endpoint)
                    .cloned();
                let name = tracked
                    .as_ref()
                    .map(|session| session.name.clone())
                    .or_else(|| {
                        state.sessions.iter().find_map(|((owner, name), session)| {
                            (owner == account && session.endpoint == runtime.endpoint)
                                .then(|| name.clone())
                        })
                    });
                if let Some(name) = &name {
                    seen.insert(name.clone());
                }
                let tunnel_name = name.as_deref().unwrap_or("").to_owned();
                rows.push(runtime_row(
                    account,
                    email.clone(),
                    active,
                    runtime,
                    name,
                    tracked,
                    tunnels(&tunnel_name),
                ));
            }
            for session in state.ledger.live_of(account) {
                if seen.contains(&session.name) {
                    continue;
                }
                if let Some(live) = state.sessions.get(&(account.clone(), session.name.clone())) {
                    let listed = ListedRuntime {
                        resource: live.resource.clone(),
                        endpoint: live.endpoint.clone(),
                        accelerator: live.accelerator,
                        high_mem: live.high_mem,
                        variant: String::new(),
                    };
                    rows.push(runtime_row(
                        account,
                        email.clone(),
                        active,
                        &listed,
                        Some(session.name.clone()),
                        Some(session.clone()),
                        tunnels(&session.name),
                    ));
                }
            }
            Ok(rows)
        })
    }

    fn usage_views(&self) -> Vec<AccountUsageView> {
        self.with_state(|state| {
            state
                .accounts
                .all()
                .iter()
                .map(|account| usage_view(state, account))
                .collect()
        })
    }

    fn public_key(&self) -> Option<String> {
        if let Some(key) = self
            .ssh_pubkey
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            return Some(key);
        }
        let home = std::env::var_os("HOME")?;
        for name in ["id_ed25519.pub", "id_ecdsa.pub"] {
            let path = PathBuf::from(&home).join(".ssh").join(name);
            if let Ok(text) = fs::read_to_string(path) {
                let line = text.lines().next().unwrap_or("").trim().to_owned();
                if line.starts_with("ssh-ed25519 ") || line.starts_with("ecdsa-sha2-") {
                    return Some(line);
                }
            }
        }
        None
    }

    fn contents_call(
        &self,
        target: &RuntimeRef,
        method: &str,
        remote: &str,
        body: Option<Value>,
        cancel: &CancelToken,
    ) -> Result<transport::HttpResponse> {
        let connection = self.connection_of(target, cancel)?;
        let base = http_base(&connection.url);
        self.api
            .contents(&base, &connection.token, method, remote, body, cancel)
            .map_err(protocol::ApiFailure::into_error)
    }

    fn put_file(
        &self,
        target: &RuntimeRef,
        remote: &str,
        bytes: &[u8],
        cancel: &CancelToken,
    ) -> Result<()> {
        let chunk = self.upload_chunk_bytes;
        let name = remote.rsplit('/').next().unwrap_or(remote);
        let send = |slice: &[u8], marker: i64| {
            let body = json!({
                "name": name,
                "path": remote,
                "type": "file",
                "format": "base64",
                "content": B64.encode(slice),
                "chunk": marker,
            });
            self.contents_call(target, "PUT", remote, Some(body), cancel)
                .map(|_| ())
        };
        if bytes.is_empty() {
            return send(&[], -1);
        }
        let mut offset = 0;
        let mut index = 1_i64;
        while offset < bytes.len() {
            cancel.check()?;
            let end = (offset + chunk).min(bytes.len());
            let last = end == bytes.len();
            let marker = if last { -1 } else { index };
            send(&bytes[offset..end], marker)?;
            offset = end;
            if !last {
                index += 1;
            }
        }
        Ok(())
    }
}

impl Backend for DirectBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Real
    }

    fn defaults(&self) -> Defaults {
        self.with_state(|state| state.defaults.clone())
    }

    fn set_defaults(&self, defaults: Defaults) {
        self.with_state(|state| state.defaults = defaults);
    }

    fn now(&self) -> SystemTime {
        self.clock.now()
    }

    fn probe(&self, cancel: &CancelToken) -> Result<ProbeReport> {
        let _ = cancel;
        let keyring_ok = self.vault.lookup("probe").is_ok();
        Ok(ProbeReport {
            wsl_installed: false,
            wsl_distros: Vec::new(),
            bash: None,
            ssh: None,
            ssh_keygen: None,
            git: None,
            uv: None,
            python3: None,
            colab: None,
            colab_version: None,
            helper_python: None,
            keyring_ok,
            ssh_key_present: self.public_key().is_some(),
            home: None,
            errors: Vec::new(),
        })
    }

    fn install_cli(
        &self,
        _method: InstallMethod,
        _log: Events<Line>,
        _cancel: &CancelToken,
    ) -> Result<()> {
        Err(unsupported("direct backend does not install the CLI"))
    }

    fn accounts(&self, cancel: &CancelToken) -> Result<AccountsView> {
        cancel.check()?;
        self.accounts_view()
    }

    fn adopt_active_account(&self, cancel: &CancelToken) -> Result<Account> {
        cancel.check()?;
        let path = self
            .adc_path
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::ActiveAccountNotAdopted))?;
        let text = fs::read_to_string(&path)?;
        let value: Value = serde_json::from_str(&text)?;
        let refresh = value
            .get("refresh_token")
            .and_then(|token| token.as_str())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::new(ErrorKind::ActiveAccountNotAdopted))?
            .to_owned();
        let oauth = self.oauth()?;
        let bundle = self
            .api
            .refresh(&oauth.client_id, &oauth.client_secret, &refresh, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        let email = self
            .api
            .userinfo(&bundle.access_token, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        let now = self.clock.now();
        let account = self.with_state(|state| {
            if let Some(existing) = state.accounts.by_email(&email).cloned() {
                existing
            } else {
                Account {
                    id: AccountId::generate(),
                    email: Some(email),
                    created_at: now,
                    updated_at: now,
                    usage: None,
                }
            }
        });
        self.vault.store(
            &account_item(&account.id),
            &json!({ "refresh_token": bundle.refresh_token }).to_string(),
        )?;
        self.remember_access(&account.id, &bundle.access_token, bundle.expires_in);
        self.with_state(|state| {
            if state.active.is_none() {
                state.active = Some(account.id.clone());
            }
            state.accounts.upsert(account.clone())
        })?;
        self.save_active()?;
        Ok(account)
    }

    fn begin_add_account(&self, cancel: &CancelToken) -> Result<Box<dyn LoginFlow>> {
        cancel.check()?;
        let oauth = self.oauth()?;
        let url = protocol::auth_url(&self.api.endpoints, &oauth.client_id);
        Ok(Box::new(DirectLogin {
            api: self.api.clone(),
            vault: self.vault.clone(),
            clock: self.clock.clone(),
            state: self.state.clone(),
            oauth: self.oauth.clone(),
            data_dir: self.data_dir.clone(),
            url,
        }))
    }

    fn switch_impact(&self, to: &AccountId) -> Result<SwitchImpact> {
        self.known_account(to)?;
        self.with_state(|state| {
            let from = state.active.clone();
            let tunnels_closing = state
                .tunnels
                .iter()
                .filter(|(_, tunnel)| Some(&tunnel.spec.account) == from.as_ref())
                .map(|(id, tunnel)| TunnelInfo {
                    id: TunnelId(*id),
                    spec: tunnel.spec.clone(),
                    state: tunnel.state.clone(),
                    healthy: None,
                })
                .collect();
            let sessions_left_running = from
                .as_ref()
                .map(|account| state.ledger.live_of(account).cloned().collect())
                .unwrap_or_default();
            Ok(SwitchImpact {
                from,
                to: to.clone(),
                tunnels_closing,
                sessions_left_running,
                unmanaged_left_running: None,
            })
        })
    }

    fn switch_account(&self, to: &AccountId, cancel: &CancelToken) -> Result<AccountsView> {
        cancel.check()?;
        self.known_account(to)?;
        let closing = self.with_state(|state| state.active.clone());
        if let Some(previous) = closing.as_ref() {
            self.close_account_tunnels(previous);
        }
        self.with_state(|state| state.active = Some(to.clone()));
        self.save_active()?;
        self.accounts_view()
    }

    fn remove_account(&self, id: &AccountId, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        self.known_account(id)?;
        let active = self.with_state(|state| state.active.clone());
        if active.as_ref() == Some(id) {
            return Err(Error::new(ErrorKind::InvalidInput)
                .with("field", "account")
                .detail("the active account cannot be removed"));
        }
        let _ = self.vault.delete(&account_item(id));
        self.with_state(|state| {
            state.access.remove(id);
            state.usage.remove(id);
            state.sessions.retain(|(owner, _), _| owner != id);
            state.accounts.remove(id)
        })
    }

    fn refresh_usage(
        &self,
        accounts: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<AccountUsageView>> {
        cancel.check()?;
        let ids = if accounts.is_empty() {
            self.with_state(|state| state.accounts.ids())
        } else {
            accounts.to_vec()
        };
        for id in &ids {
            let now = self.clock.now();
            let live = self.with_state(|state| {
                state
                    .ledger
                    .live_of(id)
                    .map(|session| session.name.clone())
                    .collect::<BTreeSet<_>>()
            });
            match self.bearer(id, cancel).and_then(|bearer| {
                self.api
                    .usage(&bearer, cancel)
                    .map_err(protocol::ApiFailure::into_error)
            }) {
                Ok(usage) => {
                    self.with_state(|state| {
                        let slot = state.usage.entry(id.clone()).or_default();
                        slot.record_success(usage, now, &live);
                    });
                }
                Err(error) => {
                    self.with_state(|state| {
                        state
                            .usage
                            .entry(id.clone())
                            .or_default()
                            .record_failure(error, now);
                    });
                }
            }
        }
        Ok(self.usage_views())
    }

    fn usage(&self) -> Vec<AccountUsageView> {
        self.usage_views()
    }

    fn usage_poll_due(&self) -> bool {
        let now = self.clock.now();
        self.with_state(|state| {
            if state.accounts.all().is_empty() {
                return false;
            }
            let has_sessions = state.ledger.all_live().next().is_some();
            state.accounts.all().iter().any(|account| {
                let last = state
                    .usage
                    .get(&account.id)
                    .and_then(|usage| usage.last_attempt);
                usage::poll_due(last, now, has_sessions, &state.defaults)
            })
        })
    }

    fn measured_rates(&self) -> Vec<sessions::HardwareRate> {
        self.with_state(|state| state.ledger.measured_rates())
    }

    fn runtimes(&self, cancel: &CancelToken) -> Result<RuntimesView> {
        self.runtime_rows(cancel)
    }

    fn hardware_catalog(&self, cancel: &CancelToken) -> Result<HardwareCatalog> {
        let account = match self.require_active() {
            Ok(account) => account,
            Err(_) => return Ok(HardwareCatalog::unknown()),
        };
        let bearer = match self.bearer(&account, cancel) {
            Ok(bearer) => bearer,
            Err(_) => return Ok(HardwareCatalog::unknown()),
        };
        let tier = match self.api.subscription(&bearer, cancel) {
            Ok(tier) => tier,
            Err(error) if error.auth_denied => return Ok(HardwareCatalog::unknown()),
            Err(error) => return Err(error.into_error()),
        };
        let offers = match self.api.specs(&bearer, cancel) {
            Ok(offers) => offers,
            Err(error) if error.auth_denied => return Ok(HardwareCatalog::unknown()),
            Err(error) => return Err(error.into_error()),
        };
        Ok(HardwareCatalog { tier, offers })
    }

    fn new_session_name(&self, prefix: Option<&str>) -> String {
        self.with_state(|state| {
            let prefix = prefix
                .map(|prefix| prefix.to_owned())
                .unwrap_or_else(|| backend::accelerator_prefix(state.defaults.accelerator));
            backend::generate_session_name(&prefix, &state.ledger.names())
        })
    }

    fn provision(
        &self,
        spec: ProvisionSpec,
        events: Events<ProvisionEvent>,
        cancel: &CancelToken,
    ) -> Result<ProvisionOutcome> {
        let account = self.require_active()?;
        let defaults = self.defaults();
        let limit = provision::SessionLimit {
            max_concurrent: defaults.max_concurrent_sessions,
            reserved: 0,
        };
        let ops = DirectOps {
            backend: self,
            account: account.clone(),
        };
        let outcome =
            provision::provision(&ops, &spec, limit, self.clock.as_ref(), cancel, &|event| {
                let _ = events.send(event);
            })?;
        let live = self.with_state(|state| {
            state
                .sessions
                .get(&(account.clone(), outcome.name.clone()))
                .map(|session| {
                    (
                        session.endpoint.clone(),
                        session.accelerator,
                        session.high_mem,
                    )
                })
        });
        let (endpoint, accelerator, high_mem) =
            live.unwrap_or((String::new(), spec.accelerator, spec.high_mem));
        let tracked = TrackedSession {
            name: outcome.name.clone(),
            account,
            endpoint: (!endpoint.is_empty()).then_some(endpoint),
            accelerator,
            high_mem,
            gpu: outcome.hardware.gpu.clone(),
            vram_mib: outcome.hardware.vram_mib,
            policy: spec.guard,
            spend: SpendMeter::new(outcome.started_at),
            watchdog: outcome.watchdog.clone(),
            stopped: None,
            measured_rate: None,
            keep_plan: None,
            last_keep_sync: None,
        };
        self.with_state(|state| state.ledger.upsert(tracked))?;
        Ok(outcome)
    }

    fn stop_session(
        &self,
        target: &RuntimeRef,
        options: StopOptions,
        cancel: &CancelToken,
    ) -> Result<()> {
        cancel.check()?;
        self.known_account(&target.account)?;
        let _ = options.save_keep;
        let bearer = self.bearer(&target.account, cancel)?;
        let located = self.locate(target);
        let Some((name, resource, endpoint)) = located else {
            return Err(Error::new(ErrorKind::SessionGone));
        };
        self.api
            .delete_runtime(&bearer, &resource, &endpoint, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        self.close_session_tunnels(&target.account, &name);
        self.with_state(|state| {
            state
                .sessions
                .remove(&(target.account.clone(), name.clone()));
            state
                .ledger
                .mark_stopped(&target.account, &name, self.clock.now(), options.reason)
        })?;
        Ok(())
    }

    fn vm_ls(&self, target: &RuntimeRef, path: &str, cancel: &CancelToken) -> Result<Vec<VmFile>> {
        let value = self.read_contents(target, path, cancel)?;
        Ok(files_from_listing(path, &value))
    }

    fn vm_list_dir(
        &self,
        target: &RuntimeRef,
        path: &str,
        cancel: &CancelToken,
    ) -> Result<Vec<VmEntry>> {
        let value = self.read_contents(target, path, cancel)?;
        let content = value
            .get("content")
            .and_then(|item| item.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(content
            .iter()
            .filter_map(|entry| {
                let name = entry.get("name").and_then(|v| v.as_str())?;
                let is_dir = entry
                    .get("type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|kind| kind == "directory");
                Some(VmEntry {
                    name: name.to_owned(),
                    is_dir,
                })
            })
            .collect())
    }

    fn vm_download(
        &self,
        target: &RuntimeRef,
        remote: &str,
        local: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        let value = self.read_contents(target, remote, cancel)?;
        let encoded = value
            .get("content")
            .and_then(|item| item.as_str())
            .ok_or_else(|| {
                Error::new(ErrorKind::Parse).detail("contents response had no content")
            })?;
        let bytes = B64.decode(encoded).map_err(|error| {
            Error::new(ErrorKind::Parse).detail(format!("contents body was not base64: {error}"))
        })?;
        if let Some(parent) = Path::new(local).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(local, bytes)?;
        Ok(())
    }

    fn vm_upload(
        &self,
        target: &RuntimeRef,
        local: &str,
        remote: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        let bytes = fs::read(local)?;
        self.put_file(target, remote, &bytes, cancel)
    }

    fn set_keep_plan(
        &self,
        target: &RuntimeRef,
        plan: Option<KeepPlan>,
        cancel: &CancelToken,
    ) -> Result<()> {
        cancel.check()?;
        let name = self.session_name(target)?;
        let updated = self.with_state(|state| {
            state.ledger.update(&target.account, &name, |session| {
                session.keep_plan = plan.clone();
            })
        })?;
        if updated {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::SessionGone))
        }
    }

    fn sync_keep(
        &self,
        _target: &RuntimeRef,
        _targets: Option<Vec<KeepTargetId>>,
        _cancel: &CancelToken,
    ) -> Result<KeepSyncReport> {
        Err(unsupported("direct backend does not sync keep plans"))
    }

    fn begin_drive_mount(
        &self,
        _target: &RuntimeRef,
        _mount_path: &str,
        _cancel: &CancelToken,
    ) -> Result<DriveMountStart> {
        Err(unsupported("direct backend does not mount Drive"))
    }

    fn extend_session(
        &self,
        target: &RuntimeRef,
        by: Duration,
        cancel: &CancelToken,
    ) -> Result<Extension> {
        let name = self.session_name(target)?;
        let runtime = self.with_state(|state| {
            let now = self.clock.now();
            state
                .ledger
                .live(&target.account, &name)
                .and_then(|session| session.auto_stop_at())
                .and_then(|at| at.duration_since(now).ok())
                .unwrap_or(Duration::ZERO)
                + by
        });
        let watchdog = self.set_auto_stop(target, runtime, cancel)?;
        Ok(Extension { runtime, watchdog })
    }

    fn set_auto_stop(
        &self,
        target: &RuntimeRef,
        runtime: Duration,
        cancel: &CancelToken,
    ) -> Result<WatchdogInfo> {
        cancel.check()?;
        let name = self.session_name(target)?;
        let policy = GuardPolicy {
            max_runtime: Some(runtime),
            idle_timeout: self.with_state(|state| {
                state
                    .ledger
                    .live(&target.account, &name)
                    .and_then(|session| session.policy.idle_timeout)
            }),
            ..GuardPolicy::unlimited()
        };
        let info = self.arm_watchdog(target, &name, &policy, cancel)?;
        let now = self.clock.now();
        self.with_state(|state| {
            state.ledger.update(&target.account, &name, |session| {
                let elapsed = now.duration_since(session.started_at()).unwrap_or_default();
                session.policy.max_runtime = Some(elapsed + runtime);
                session.watchdog = Some(info.clone());
            })
        })?;
        Ok(info)
    }

    fn cancel_auto_stop(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        let name = self.session_name(target)?;
        let _ = self.put_file(target, "/tmp/cpg-watchdog-cancel", b"1", cancel);
        self.with_state(|state| {
            state.ledger.update(&target.account, &name, |session| {
                session.policy.max_runtime = None;
                session.watchdog = Some(WatchdogInfo {
                    deadline: None,
                    idle_timeout: session.policy.idle_timeout,
                    cancelled: true,
                    armed: false,
                });
            })
        })?;
        Ok(())
    }

    fn guard_poll(&self, cancel: &CancelToken) -> Result<GuardReport> {
        cancel.check()?;
        let _ = self.refresh_usage(&[], cancel);
        let targets: Vec<(RuntimeRef, crate::guard::Decision)> = self.with_state(|state| {
            let now = self.clock.now();
            state
                .ledger
                .all_live()
                .map(|session| {
                    let usage = state
                        .usage
                        .get(&session.account)
                        .and_then(|usage| usage.snapshot);
                    let decision =
                        guard::evaluate(&session.policy, now, usage.as_ref(), &session.spend);
                    (
                        RuntimeRef {
                            account: session.account.clone(),
                            endpoint: session.endpoint.clone().unwrap_or_default(),
                            name: Some(session.name.clone()),
                        },
                        decision,
                    )
                })
                .collect()
        });
        let mut decisions = Vec::new();
        for (target, decision) in targets {
            let mut stopped = false;
            let mut error = None;
            if let crate::guard::Decision::Stop(concern) = &decision {
                match self.stop_session(
                    &target,
                    StopOptions {
                        reason: StopReason::Guard {
                            concern: concern.clone(),
                        },
                        ..StopOptions::default()
                    },
                    cancel,
                ) {
                    Ok(()) => stopped = true,
                    Err(stop_error) => error = Some(stop_error),
                }
            }
            decisions.push(GuardDecision {
                session: target,
                decision,
                stopped,
                error,
            });
        }
        Ok(GuardReport {
            decisions,
            vanished: Vec::new(),
            usage: self.usage_views(),
        })
    }

    fn open_tunnel(&self, spec: TunnelSpec, events: Events<TunnelEvent>) -> Result<TunnelId> {
        spec.validate()?;
        self.known_account(&spec.account)?;
        if crate::http::local_port_in_use(spec.local_port, Duration::from_millis(200)) {
            return Err(Error::new(ErrorKind::PortInUse).with("port", spec.local_port));
        }
        let target = RuntimeRef {
            account: spec.account.clone(),
            endpoint: String::new(),
            name: Some(spec.session.clone()),
        };
        let cancel = CancelToken::new();
        let connection = self.connection_of(&target, &cancel)?;
        let pubkey = self.public_key().ok_or_else(|| {
            Error::invalid("ssh_pubkey", "an ed25519 or ecdsa public key is required")
        })?;
        if pubkey.starts_with("ssh-rsa ") {
            return Err(Error::invalid("ssh_pubkey", "rsa keys are not accepted"));
        }
        let ws_url = ws_url(
            &connection.url,
            &format!(
                "/colab/ssh?colab-runtime-proxy-token={}&colab-client-agent={}&colab-client-agent=ssh-mode-proxy",
                protocol::encode(&connection.token),
                protocol::encode(protocol::CLIENT_AGENT)
            ),
        )?;
        let socket = self
            .ws
            .open(
                &ws_url,
                &[
                    ("X-Colab-Ssh-Pubkey".into(), pubkey),
                    (
                        "X-Colab-Runtime-Proxy-Token".into(),
                        connection.token.clone(),
                    ),
                    ("X-Colab-Client-Agent".into(), protocol::CLIENT_AGENT.into()),
                ],
                Duration::from_secs(10),
            )
            .map_err(|error| {
                self.api.record(
                    "GET",
                    "/colab/ssh",
                    error
                        .params
                        .get("status")
                        .and_then(|status| status.parse().ok())
                        .unwrap_or(0),
                );
                error
            })?;
        self.api.record("GET", "/colab/ssh", 101);
        // One websocket is the SSH transport. OpenSSH `-L` opens a channel per local
        // connection to 127.0.0.1:remote_port on the runtime. Bytes are not echoed.
        let transport = TcpListener::bind(("127.0.0.1", 0))?;
        transport.set_nonblocking(true)?;
        let transport_port = transport.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(socket));
        let stop_loop = stop.clone();
        thread::spawn(move || accept_ssh_transport(transport, socket, stop_loop));
        let ssh = match spawn_ssh_forward(
            &spec,
            transport_port,
            self.ssh_identity.as_deref(),
            &self.ssh_user,
        ) {
            Ok(child) => child,
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                return Err(error);
            }
        };
        let id = self.with_state(|state| {
            let id = state.next_tunnel;
            state.next_tunnel += 1;
            state.tunnels.insert(
                id,
                LiveTunnel {
                    spec: spec.clone(),
                    state: TunnelState::Up,
                    stop: stop.clone(),
                    ssh: Some(ssh),
                },
            );
            id
        });
        let tunnel_id = TunnelId(id);
        let _ = events.send(TunnelEvent::State {
            id: tunnel_id,
            state: TunnelState::Up,
        });
        Ok(tunnel_id)
    }

    fn close_tunnel(&self, id: TunnelId) -> Result<()> {
        let stopped = self.with_state(|state| {
            state.tunnels.get_mut(&id.0).map(|tunnel| {
                halt_tunnel(tunnel);
            })
        });
        if stopped.is_some() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::SessionGone).detail("tunnel is not open"))
        }
    }

    fn tunnels(&self) -> Vec<TunnelInfo> {
        self.with_state(|state| {
            state
                .tunnels
                .iter()
                .map(|(id, tunnel)| TunnelInfo {
                    id: TunnelId(*id),
                    spec: tunnel.spec.clone(),
                    state: tunnel.state.clone(),
                    healthy: None,
                })
                .collect()
        })
    }

    fn open_in_browser(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<String> {
        cancel.check()?;
        self.known_account(&target.account)?;
        let _ = self
            .locate(target)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        Ok("https://colab.research.google.com/".to_owned())
    }

    fn restart_kernel(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        let connection = self.connection_of(target, cancel)?;
        let base = http_base(&connection.url);
        self.api
            .kernel_http(
                &base,
                &connection.token,
                "/api/kernels/restart",
                "POST",
                cancel,
            )
            .map_err(protocol::ApiFailure::into_error)
    }

    fn reset_ssh_sessions(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        let name = self.session_name(target)?;
        self.close_session_tunnels(&target.account, &name);
        Ok(())
    }

    fn remote_ssh(&self, _target: &RuntimeRef, _cancel: &CancelToken) -> Result<RemoteSsh> {
        Err(unsupported("token cannot be written into an ssh config"))
    }

    fn write_temp_ssh_config(
        &self,
        _target: &RuntimeRef,
        _cancel: &CancelToken,
    ) -> Result<PathBuf> {
        Err(unsupported("token cannot be written into an ssh config"))
    }

    fn plugins(&self) -> Vec<PluginInfo> {
        Vec::new()
    }

    fn plugin_run_template(&self, _plugin: &str) -> Result<PluginRunSpec> {
        Err(unsupported("direct backend does not run plugins"))
    }

    fn inspect_plugin_folder(&self, _path: &Path) -> Result<PluginPreview> {
        Err(unsupported("direct backend does not install plugins"))
    }

    fn inspect_plugin_git(&self, _url: &str, _cancel: &CancelToken) -> Result<PluginPreview> {
        Err(unsupported("direct backend does not install plugins"))
    }

    fn install_plugin(&self, _preview: &PluginPreview) -> Result<PluginInfo> {
        Err(unsupported("direct backend does not install plugins"))
    }

    fn discard_plugin_preview(&self, _preview: &PluginPreview) {}

    fn set_plugin_enabled(&self, _id: &str, _enabled: bool) -> Result<()> {
        Err(unsupported("direct backend does not install plugins"))
    }

    fn remove_plugin(&self, _id: &str) -> Result<()> {
        Err(unsupported("direct backend does not install plugins"))
    }

    fn run_plugin(
        &self,
        _spec: PluginRunSpec,
        _events: Events<PluginRunEvent>,
        _cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        Err(unsupported("direct backend does not run plugins"))
    }

    fn set_secret(&self, name: &str, value: &str, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        validate_secret_name(name)?;
        self.vault.store(&secret_item(name), value)
    }

    fn has_secret(&self, name: &str, cancel: &CancelToken) -> Result<bool> {
        cancel.check()?;
        validate_secret_name(name)?;
        Ok(self.vault.lookup(&secret_item(name))?.is_some())
    }

    fn delete_secret(&self, name: &str, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        validate_secret_name(name)?;
        self.vault.delete(&secret_item(name))
    }

    fn mcp_token(&self, cancel: &CancelToken) -> Result<String> {
        cancel.check()?;
        if let Some(token) = self.vault.lookup(&secret_item("mcp_token"))? {
            return Ok(token);
        }
        let token = format!("{}{}", crate::accounts::nonce(), crate::accounts::nonce());
        self.vault.store(&secret_item("mcp_token"), &token)?;
        Ok(token)
    }

    fn rotate_mcp_token(&self, cancel: &CancelToken) -> Result<String> {
        cancel.check()?;
        let token = format!("{}{}", crate::accounts::nonce(), crate::accounts::nonce());
        self.vault.store(&secret_item("mcp_token"), &token)?;
        Ok(token)
    }

    fn hf_search(&self, _query: &str, _limit: u32, _cancel: &CancelToken) -> Result<Vec<HfModel>> {
        Err(unsupported("direct backend does not search hugging face"))
    }

    fn hf_files(&self, _repo: &str, _cancel: &CancelToken) -> Result<GgufListing> {
        Err(unsupported("direct backend does not search hugging face"))
    }
}

#[cfg(test)]
impl DirectBackend {
    /// Test-only: a saved account whose access token is already in memory.
    pub(crate) fn seed_account(
        &self,
        email: &str,
        access: &str,
        refresh: &str,
        active: bool,
    ) -> Result<AccountId> {
        let id = AccountId::generate();
        let now = self.clock.now();
        self.vault.store(
            &account_item(&id),
            &json!({ "refresh_token": refresh }).to_string(),
        )?;
        self.with_state(|state| {
            state.accounts.upsert(Account {
                id: id.clone(),
                email: Some(email.to_owned()),
                created_at: now,
                updated_at: now,
                usage: None,
            })?;
            state.access.insert(
                id.clone(),
                Access {
                    token: access.to_owned(),
                    expires_at: now + Duration::from_secs(3600),
                },
            );
            if active {
                state.active = Some(id.clone());
            }
            Ok(id)
        })
    }

    /// Test-only: an in-memory runtime plus a ledger row. The proxy token stays in memory.
    pub(crate) fn remember_runtime(
        &self,
        account: &AccountId,
        name: &str,
        resource: &str,
        endpoint: &str,
        accelerator: Accelerator,
        token: &str,
        url: &str,
    ) -> Result<()> {
        let now = self.clock.now();
        self.with_state(|state| {
            state.sessions.insert(
                (account.clone(), name.to_owned()),
                LiveSession {
                    resource: resource.to_owned(),
                    endpoint: endpoint.to_owned(),
                    accelerator,
                    high_mem: false,
                    connection: Some(ConnectionInfo {
                        url: url.to_owned(),
                        token: token.to_owned(),
                        expire_time: None,
                        resource: resource.to_owned(),
                        endpoint: endpoint.to_owned(),
                    }),
                },
            );
            state.ledger.upsert(TrackedSession {
                name: name.to_owned(),
                account: account.clone(),
                endpoint: Some(endpoint.to_owned()),
                accelerator,
                high_mem: false,
                gpu: None,
                vram_mib: None,
                policy: GuardPolicy::unlimited(),
                spend: SpendMeter::new(now),
                watchdog: None,
                stopped: None,
                measured_rate: None,
                keep_plan: None,
                last_keep_sync: None,
            })
        })
    }
}

struct DirectLogin {
    api: Api,
    vault: Arc<dyn TokenVault>,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
    oauth: Arc<Mutex<Option<OAuthClient>>>,
    data_dir: Option<PathBuf>,
    url: String,
}

impl LoginFlow for DirectLogin {
    fn auth_url(&self) -> &str {
        &self.url
    }

    fn submit_code(self: Box<Self>, code: &str, cancel: &CancelToken) -> Result<Account> {
        cancel.check()?;
        let oauth = self
            .oauth
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| Error::invalid("oauth", "oauth client config is missing"))?;
        let bundle = self
            .api
            .exchange_code(&oauth.client_id, &oauth.client_secret, code, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        if bundle.refresh_token.is_empty() {
            return Err(Error::new(ErrorKind::LoginVerificationFailed)
                .detail("token response had no refresh token"));
        }
        let email = self
            .api
            .userinfo(&bundle.access_token, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        let now = self.clock.now();
        let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut account = guard.accounts.by_email(&email).cloned().unwrap_or(Account {
            id: AccountId::generate(),
            email: Some(email),
            created_at: now,
            updated_at: now,
            usage: None,
        });
        account.email = Some(account.email.clone().unwrap_or_else(|| "unknown".into()));
        account.updated_at = now;
        self.vault.store(
            &account_item(&account.id),
            &json!({ "refresh_token": bundle.refresh_token }).to_string(),
        )?;
        guard.access.insert(
            account.id.clone(),
            Access {
                token: bundle.access_token,
                expires_at: now + Duration::from_secs(bundle.expires_in.max(60)),
            },
        );
        if guard.active.is_none() {
            guard.active = Some(account.id.clone());
        }
        guard.accounts.upsert(account.clone())?;
        drop(guard);
        if let Some(dir) = &self.data_dir {
            let active = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active
                .as_ref()
                .map(|id| id.as_str().to_owned());
            let bytes = serde_json::to_vec_pretty(&json!({ "active": active }))?;
            write_atomic(&dir.join("direct.json"), &bytes)?;
        }
        Ok(account)
    }

    fn cancel(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

struct DirectOps<'a> {
    backend: &'a DirectBackend,
    account: AccountId,
}

impl provision::ProvisionOps for DirectOps<'_> {
    fn sessions(&self, cancel: &CancelToken) -> Result<Vec<ServerSession>> {
        let listed = self.backend.refresh_account(&self.account, cancel)?;
        self.backend.with_state(|state| {
            let mut rows = Vec::new();
            let mut seen = BTreeSet::new();
            for runtime in &listed {
                let name = state
                    .ledger
                    .live_by_endpoint(&self.account, &runtime.endpoint)
                    .map(|session| session.name.clone())
                    .or_else(|| {
                        state.sessions.iter().find_map(|((owner, name), session)| {
                            (owner == &self.account && session.endpoint == runtime.endpoint)
                                .then(|| name.clone())
                        })
                    });
                if let Some(name) = &name {
                    seen.insert(name.clone());
                }
                seen.insert(runtime.endpoint.clone());
                rows.push(server_session(name, runtime));
            }
            for ((owner, name), session) in &state.sessions {
                if owner != &self.account || seen.contains(name) || seen.contains(&session.endpoint)
                {
                    continue;
                }
                rows.push(ServerSession {
                    name: Some(name.clone()),
                    endpoint: session.endpoint.clone(),
                    hardware: hardware_label(session.accelerator),
                    accelerator: Some(session.accelerator),
                    shape: shape_label(session.high_mem),
                    variant: variant_label(session.accelerator),
                    status: Some("running".into()),
                });
            }
            Ok(rows)
        })
    }

    fn create(
        &self,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        let bearer = self.backend.bearer(&self.account, cancel)?;
        if let Ok(offers) = self.backend.api.specs(&bearer, cancel) {
            let catalog = HardwareCatalog { tier: None, offers };
            if catalog.blocks(accelerator, high_mem) {
                return Err(Error::new(ErrorKind::NotEntitled));
            }
        }
        let created = self
            .backend
            .api
            .create_runtime(&bearer, accelerator, high_mem, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        self.backend.with_state(|state| {
            let connection = created.connection.clone();
            state.sessions.insert(
                (self.account.clone(), name.to_owned()),
                LiveSession {
                    resource: created.listed.resource.clone(),
                    endpoint: created.listed.endpoint.clone(),
                    accelerator,
                    high_mem: created.listed.high_mem,
                    connection,
                },
            );
        });
        Ok(())
    }

    fn probe(&self, name: &str, cancel: &CancelToken) -> Result<Hardware> {
        let accelerator = self.backend.with_state(|state| {
            state
                .sessions
                .get(&(self.account.clone(), name.to_owned()))
                .map(|session| session.accelerator)
        });
        if !accelerator.is_some_and(Accelerator::is_gpu) {
            return Ok(Hardware::default());
        }
        let target = self.named(name);
        let output = self.backend.execute_code(&target, PROBE_CODE, cancel)?;
        Ok(parse_probe(&output))
    }

    fn stop(&self, name: &str, cancel: &CancelToken) -> Result<crate::cli::StopOutcome> {
        let target = self.named(name);
        self.backend
            .stop_session(&target, StopOptions::default(), cancel)?;
        Ok(crate::cli::StopOutcome::Terminated)
    }

    fn reap(&self, endpoint: &str, cancel: &CancelToken) -> Result<bool> {
        let bearer = self.backend.bearer(&self.account, cancel)?;
        let resource = self.backend.with_state(|state| {
            state.sessions.iter().find_map(|((owner, _), session)| {
                (owner == &self.account && session.endpoint == endpoint)
                    .then(|| session.resource.clone())
            })
        });
        let resource = resource.unwrap_or_else(|| format!("runtimes/{endpoint}"));
        self.backend
            .api
            .delete_runtime(&bearer, &resource, endpoint, cancel)
            .map_err(protocol::ApiFailure::into_error)?;
        self.backend.with_state(|state| {
            state
                .sessions
                .retain(|(_, _), session| session.endpoint != endpoint);
        });
        Ok(true)
    }

    fn start_watchdog(
        &self,
        name: &str,
        policy: &GuardPolicy,
        cancel: &CancelToken,
    ) -> Result<Option<WatchdogInfo>> {
        let target = self.named(name);
        let code = watchdog_code(policy, self.backend.clock.now());
        let output = self.backend.execute_code(&target, &code, cancel)?;
        if !output.contains("CPG_WATCHDOG armed") {
            return Err(Error::new(ErrorKind::Other).detail("watchdog did not arm"));
        }
        Ok(Some(WatchdogInfo {
            deadline: policy
                .max_runtime
                .map(|runtime| self.backend.clock.now() + runtime),
            idle_timeout: policy.idle_timeout,
            cancelled: false,
            armed: true,
        }))
    }
}

impl DirectOps<'_> {
    fn named(&self, name: &str) -> RuntimeRef {
        let endpoint = self.backend.with_state(|state| {
            state
                .sessions
                .get(&(self.account.clone(), name.to_owned()))
                .map(|session| session.endpoint.clone())
                .unwrap_or_default()
        });
        RuntimeRef {
            account: self.account.clone(),
            endpoint,
            name: Some(name.to_owned()),
        }
    }
}

impl DirectBackend {
    fn locate(&self, target: &RuntimeRef) -> Option<(String, String, String)> {
        self.with_state(|state| {
            if let Some(name) = &target.name {
                if let Some(session) = state.sessions.get(&(target.account.clone(), name.clone())) {
                    return Some((
                        name.clone(),
                        session.resource.clone(),
                        session.endpoint.clone(),
                    ));
                }
                if let Some(tracked) = state.ledger.live(&target.account, name) {
                    let endpoint = tracked.endpoint.clone().unwrap_or_default();
                    let resource = state
                        .sessions
                        .get(&(target.account.clone(), name.clone()))
                        .map(|session| session.resource.clone())
                        .unwrap_or_else(|| {
                            if endpoint.is_empty() {
                                String::new()
                            } else {
                                format!("runtimes/{endpoint}")
                            }
                        });
                    return Some((name.clone(), resource, endpoint));
                }
            }
            if !target.endpoint.is_empty() {
                if let Some(((_, name), session)) =
                    state.sessions.iter().find(|((owner, _), session)| {
                        owner == &target.account && session.endpoint == target.endpoint
                    })
                {
                    return Some((
                        name.clone(),
                        session.resource.clone(),
                        session.endpoint.clone(),
                    ));
                }
                if let Some(tracked) = state
                    .ledger
                    .live_by_endpoint(&target.account, &target.endpoint)
                {
                    return Some((
                        tracked.name.clone(),
                        format!("runtimes/{}", target.endpoint),
                        target.endpoint.clone(),
                    ));
                }
                if let Some(slot) = state
                    .listed
                    .get(&(target.account.clone(), target.endpoint.clone()))
                {
                    let name = target
                        .name
                        .clone()
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| slot.endpoint.clone());
                    return Some((name, slot.resource.clone(), slot.endpoint.clone()));
                }
            }
            None
        })
    }

    fn session_name(&self, target: &RuntimeRef) -> Result<String> {
        self.locate(target)
            .map(|(name, _, _)| name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))
    }

    fn close_account_tunnels(&self, account: &AccountId) {
        self.with_state(|state| {
            for tunnel in state.tunnels.values_mut() {
                if &tunnel.spec.account == account {
                    halt_tunnel(tunnel);
                }
            }
        });
    }

    fn close_session_tunnels(&self, account: &AccountId, name: &str) {
        self.with_state(|state| {
            for tunnel in state.tunnels.values_mut() {
                if &tunnel.spec.account == account && tunnel.spec.session == name {
                    halt_tunnel(tunnel);
                }
            }
        });
    }

    fn read_contents(
        &self,
        target: &RuntimeRef,
        path: &str,
        cancel: &CancelToken,
    ) -> Result<Value> {
        let response = self.contents_call(target, "GET", path, None, cancel)?;
        protocol::parse_json(&response.body)
    }

    fn arm_watchdog(
        &self,
        target: &RuntimeRef,
        name: &str,
        policy: &GuardPolicy,
        cancel: &CancelToken,
    ) -> Result<WatchdogInfo> {
        let ops = DirectOps {
            backend: self,
            account: target.account.clone(),
        };
        let _ = name;
        ops.start_watchdog(&ops.named_name(target), policy, cancel)?
            .ok_or_else(|| Error::new(ErrorKind::Other).detail("watchdog did not arm"))
    }
}

impl DirectOps<'_> {
    fn named_name(&self, target: &RuntimeRef) -> String {
        target.name.clone().unwrap_or_default()
    }
}

fn halt_tunnel(tunnel: &mut LiveTunnel) {
    tunnel.stop.store(true, Ordering::Relaxed);
    tunnel.state = TunnelState::Stopped;
    if let Some(ssh) = tunnel.ssh.as_mut() {
        if let Some(child) = ssh.0.as_mut() {
            let _ = child.kill();
        }
    }
}

/// Accepts the one OpenSSH client and bridges it to the runtime websocket.
fn accept_ssh_transport(
    listener: TcpListener,
    socket: Arc<Mutex<Box<dyn WsConn>>>,
    stop: Arc<AtomicBool>,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                bridge_one(stream, socket, stop);
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
}

fn ssh_bin() -> &'static str {
    if Path::new("/usr/bin/ssh").exists() {
        "/usr/bin/ssh"
    } else {
        "ssh"
    }
}

/// `ssh -N -L 127.0.0.1:local:127.0.0.1:remote` through the websocket bridge.
fn spawn_ssh_forward(
    spec: &TunnelSpec,
    transport_port: u16,
    identity: Option<&Path>,
    user: &str,
) -> Result<SshProcess> {
    let mut command = std::process::Command::new(ssh_bin());
    command
        .arg("-N")
        .arg("-T")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("GlobalKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-p")
        .arg(transport_port.to_string())
        .arg("-L")
        .arg(format!(
            "127.0.0.1:{}:127.0.0.1:{}",
            spec.local_port, spec.remote_port
        ));
    if let Some(path) = identity {
        command.arg("-i").arg(path);
        command.arg("-o").arg("IdentitiesOnly=yes");
    }
    command.arg(format!("{user}@127.0.0.1"));
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        Error::new(ErrorKind::NotInstalled).detail(format!("ssh failed to start: {error}"))
    })?;
    let log = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let log = log.clone();
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            let mut slot = log.lock().unwrap_or_else(|error| error.into_inner());
            slot.push_str(&buf);
            if slot.len() > 1500 {
                slot.truncate(1500);
            }
        });
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(Error::from)? {
            thread::sleep(Duration::from_millis(20));
            let detail = log
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            return Err(Error::new(ErrorKind::Io).detail(format!("ssh exited {status}: {detail}")));
        }
        if crate::http::local_port_in_use(spec.local_port, Duration::from_millis(40)) {
            return Ok(SshProcess(Some(child)));
        }
        thread::sleep(Duration::from_millis(30));
    }
    let _ = child.kill();
    let detail = log
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    Err(Error::new(ErrorKind::Timeout).detail(format!("ssh forward did not listen: {detail}")))
}

fn bridge_one(
    mut stream: std::net::TcpStream,
    socket: Arc<Mutex<Box<dyn WsConn>>>,
    stop: Arc<AtomicBool>,
) {
    // Read and write use cloned sockets so a blocking read cannot hold the echo back.
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(mut incoming) = stream.try_clone() else {
        return;
    };
    let _ = incoming.set_nodelay(true);
    let _ = incoming.set_nonblocking(false);
    let _ = incoming.set_read_timeout(Some(Duration::from_millis(50)));
    let stop_in = stop.clone();
    let socket_in = socket.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while !stop_in.load(Ordering::Relaxed) {
            match incoming.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let payload = buf[..n].to_vec();
                    loop {
                        if stop_in.load(Ordering::Relaxed) {
                            return;
                        }
                        let sent = {
                            let mut socket =
                                socket_in.lock().unwrap_or_else(|error| error.into_inner());
                            socket.send(0x2, &payload)
                        };
                        match sent {
                            Ok(()) => break,
                            Err(error) if is_timeout(&error) => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => return,
                        }
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            }
        }
    });
    while !stop.load(Ordering::Relaxed) {
        let frame = {
            let mut socket = socket.lock().unwrap_or_else(|error| error.into_inner());
            socket.recv(Duration::from_millis(200))
        };
        match frame {
            Ok((0x8, _)) => break,
            Ok((0x9, payload)) => {
                let mut socket = socket.lock().unwrap_or_else(|error| error.into_inner());
                let _ = socket.send(0xA, &payload);
            }
            Ok((_, payload)) => {
                if stream.write_all(&payload).is_err() {
                    break;
                }
            }
            Err(error) if is_timeout(&error) => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => break,
        }
    }
}

fn server_session(name: Option<String>, runtime: &ListedRuntime) -> ServerSession {
    ServerSession {
        name,
        endpoint: runtime.endpoint.clone(),
        hardware: hardware_label(runtime.accelerator),
        accelerator: Some(runtime.accelerator),
        shape: shape_label(runtime.high_mem),
        variant: variant_label(runtime.accelerator),
        status: Some("running".into()),
    }
}

fn hardware_label(accelerator: Accelerator) -> String {
    if accelerator == Accelerator::None {
        "CPU".into()
    } else {
        accelerator.as_str().into()
    }
}

fn shape_label(high_mem: bool) -> String {
    if high_mem {
        "High-RAM".into()
    } else {
        "Standard".into()
    }
}

fn variant_label(accelerator: Accelerator) -> String {
    if accelerator.is_tpu() {
        "TPU".into()
    } else if accelerator.is_gpu() {
        "GPU".into()
    } else {
        "DEFAULT".into()
    }
}

fn parse_probe(output: &str) -> Hardware {
    let mut gpu = None;
    let mut vram_mib = None;
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("CPG_VRAM ") {
            vram_mib = rest.trim().parse().ok();
        }
        if let Some(rest) = line.trim().strip_prefix("CPG_GPU ") {
            let name = rest.trim();
            if !name.is_empty() {
                gpu = Some(name.to_owned());
            }
        }
    }
    Hardware { gpu, vram_mib }
}

fn watchdog_code(policy: &GuardPolicy, now: SystemTime) -> String {
    let deadline = policy
        .max_runtime
        .map(|runtime| now + runtime)
        .and_then(|when| when.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|runtime| runtime.as_secs())
        .unwrap_or(0);
    format!(
        "import os, threading, time, urllib.request\n\
deadline = {deadline}\n\
marker = '/tmp/cpg-watchdog-cancel'\n\
def _cpg_watchdog():\n\
    while time.time() < deadline:\n\
        if os.path.exists(marker):\n\
            return\n\
        time.sleep(5)\n\
    addr = os.environ.get('TBE_RUNTIME_ADDR', '')\n\
    if addr:\n\
        urllib.request.urlopen('http://%s/unassign' % addr, data=b'')\n\
threading.Thread(target=_cpg_watchdog, daemon=True).start()\n\
print('CPG_WATCHDOG armed')\n"
    )
}

fn runtime_row(
    account: &AccountId,
    email: Option<String>,
    active: Option<&AccountId>,
    runtime: &ListedRuntime,
    name: Option<String>,
    tracked: Option<TrackedSession>,
    tunnels: Vec<TunnelId>,
) -> Runtime {
    let high_mem = tracked
        .as_ref()
        .map(|session| session.high_mem)
        .unwrap_or(runtime.high_mem);
    let accelerator = tracked
        .as_ref()
        .map(|session| session.accelerator)
        .unwrap_or(runtime.accelerator);
    Runtime {
        id: RuntimeRef {
            account: account.clone(),
            endpoint: runtime.endpoint.clone(),
            name: name.clone(),
        },
        account_email: email,
        on_active_account: Some(account) == active,
        accelerator: Some(accelerator),
        hardware: hardware_label(accelerator),
        high_mem,
        managed: tracked.is_some(),
        auto_stop_at: tracked.as_ref().and_then(TrackedSession::auto_stop_at),
        tunnels,
        estimated_rate_cu_per_hour: Some(published_rate(accelerator, high_mem)),
        estimated_spend_cu: tracked.as_ref().map(|session| session.spend.spend_cu),
        tracked,
    }
}

fn usage_view(state: &State, account: &Account) -> AccountUsageView {
    let usage = state.usage.get(&account.id);
    let managed = state.ledger.live_of(&account.id).count();
    AccountUsageView {
        account: account.id.clone(),
        email: account.email.clone(),
        snapshot: usage.and_then(|usage| usage.snapshot),
        eta: usage
            .and_then(|usage| usage.snapshot)
            .and_then(|snapshot| snapshot.eta()),
        history: usage
            .map(|usage| usage.history.points())
            .unwrap_or_default(),
        unmanaged_runtimes: usage
            .map(|usage| usage.unmanaged_runtimes(managed))
            .unwrap_or(0),
        ownership_verified: usage.is_some_and(|usage| usage.ownership_verified),
        session_rates: usage
            .map(|usage| usage.attribution.rates())
            .unwrap_or_default(),
        last_error: usage.and_then(|usage| usage.last_error.clone()),
    }
}

fn session_for<'a>(state: &'a State, target: &RuntimeRef) -> Option<&'a LiveSession> {
    if let Some(name) = &target.name {
        if let Some(session) = state.sessions.get(&(target.account.clone(), name.clone())) {
            return Some(session);
        }
    }
    if !target.endpoint.is_empty() {
        return state
            .sessions
            .values()
            .find(|session| session.endpoint == target.endpoint);
    }
    None
}

fn attach_connection(
    state: &mut State,
    account: &AccountId,
    runtime: &protocol::ParsedRuntime,
    connection: &ConnectionInfo,
) {
    let name = state
        .ledger
        .live_by_endpoint(account, &runtime.listed.endpoint)
        .map(|session| session.name.clone())
        .or_else(|| {
            state.sessions.iter().find_map(|((owner, name), session)| {
                (owner == account && session.endpoint == runtime.listed.endpoint)
                    .then(|| name.clone())
            })
        });
    let Some(name) = name else {
        return;
    };
    let slot = state
        .sessions
        .entry((account.clone(), name))
        .or_insert_with(|| LiveSession {
            resource: runtime.listed.resource.clone(),
            endpoint: runtime.listed.endpoint.clone(),
            accelerator: runtime.listed.accelerator,
            high_mem: runtime.listed.high_mem,
            connection: None,
        });
    slot.resource = runtime.listed.resource.clone();
    slot.endpoint = runtime.listed.endpoint.clone();
    if connection.token.is_empty() {
        return;
    }
    slot.connection = Some(connection.clone());
}

fn files_from_listing(path: &str, value: &Value) -> Vec<VmFile> {
    let kind = value
        .get("type")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    if kind == "file" {
        let size = value
            .get("size")
            .and_then(|item| item.as_u64())
            .unwrap_or(0);
        return vec![VmFile {
            path: path.to_owned(),
            size,
        }];
    }
    value
        .get("content")
        .and_then(|item| item.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(|item| item.as_str())?;
                    let size = entry
                        .get("size")
                        .and_then(|item| item.as_u64())
                        .unwrap_or(0);
                    let child = if path.is_empty() || path == "/" {
                        name.to_owned()
                    } else {
                        format!("{}/{}", path.trim_end_matches('/'), name)
                    };
                    Some(VmFile { path: child, size })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn read_active(path: &Path) -> Option<AccountId> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let id = value.get("active").and_then(|item| item.as_str())?;
    AccountId::new(id).ok()
}

fn read_oauth(path: &Path) -> Result<OAuthClient> {
    let text = fs::read_to_string(path)
        .map_err(|_| Error::invalid("oauth", "oauth client config could not be read"))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| Error::invalid("oauth", "oauth client config could not be read"))?;
    let client_id = value
        .get("client_id")
        .and_then(|item| item.as_str())
        .unwrap_or("")
        .to_owned();
    let client_secret = value
        .get("client_secret")
        .and_then(|item| item.as_str())
        .unwrap_or("")
        .to_owned();
    if client_id.is_empty() || client_secret.is_empty() {
        return Err(Error::invalid("oauth", "oauth client config is incomplete"));
    }
    Ok(OAuthClient {
        client_id,
        client_secret,
    })
}

fn refresh_from_stored(stored: &str) -> Result<String> {
    let value: Value = serde_json::from_str(stored).unwrap_or(Value::Null);
    if let Some(token) = value.get("refresh_token").and_then(|item| item.as_str()) {
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
    }
    if stored.trim().is_empty() {
        Err(Error::new(ErrorKind::AuthRequired))
    } else {
        Ok(stored.to_owned())
    }
}

fn ws_url(base: &str, path_and_query: &str) -> Result<String> {
    let http = http_base(base);
    if let Some(rest) = http.strip_prefix("https://") {
        return Ok(format!("wss://{rest}{path_and_query}"));
    }
    if let Some(rest) = http.strip_prefix("http://") {
        return Ok(format!("ws://{rest}{path_and_query}"));
    }
    Err(Error::new(ErrorKind::InvalidInput)
        .with("field", "url")
        .detail("runtime url is not http"))
}

fn http_base(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    if let Some(index) = without_query.find("://") {
        if let Some(slash) = without_query[index + 3..].find('/') {
            return without_query[..index + 3 + slash].to_owned();
        }
    }
    without_query.trim_end_matches('/').to_owned()
}

fn is_timeout(error: &Error) -> bool {
    error.kind == ErrorKind::Io && {
        let detail = error.detail.to_ascii_lowercase();
        detail.contains("timed out")
            || detail.contains("would block")
            || detail.contains("temporarily unavailable")
    }
}

fn unsupported(detail: &str) -> Error {
    Error::new(ErrorKind::Unsupported).detail(detail)
}
