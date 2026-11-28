//! What an agent may read, and where it comes from.
//!
//! The endpoint never reaches into the app: it asks a [`Source`] for a [`Snapshot`] and answers
//! from that. The app holds the state the window renders, so a snapshot taken there is the same
//! truth the interface shows — no second cache, nothing that can drift.
//!
//! The shapes here are the endpoint's contract with a client. They are deliberately plain:
//! strings for anything a person reads (a stage name, an address), numbers for money and ports,
//! and no optional field that a missing value could make ambiguous — an absent run is an error
//! from the tool, not a run with `null` in it.

use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};

/// A view of the app at one moment. Everything a read tool can answer is taken from here.
pub trait Source: Send + Sync {
    fn snapshot(&self) -> Snapshot;
}

/// The state the read tools describe.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub usage: Vec<Usage>,
    pub runs: Vec<Run>,
    pub plugins: Vec<Plugin>,
    pub accounts: Vec<Account>,
    /// Plugin runs whose manifest mode is job. A service run stays on [`Snapshot::runs`].
    pub jobs: Vec<Job>,
    /// Local connection notes for a forwarded service. Loopback addresses only.
    pub downstream: Vec<Downstream>,
    pub forwards: Vec<Forward>,
    pub settings: AppSettings,
    /// What the app could not tell: a listing that has not come back, an account it could not
    /// read. A tool that answers an empty list says so here, because an agent must never read
    /// "nothing is running" out of "the app has not looked yet".
    pub notes: Vec<String>,
}

/// One runtime on one account.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Session {
    pub name: String,
    /// The account it belongs to (the address signed in with).
    pub account: String,
    /// The accelerator, as the app names it (`T4`, `A100`, ...).
    pub hardware: String,
    /// The runtime's state in the app's own words (`running`, `starting`, ...).
    pub state: String,
    /// When it stops by itself, as the app shows it (`30 min`, `never`, ...).
    pub auto_stop: String,
    /// Compute units per hour, when the app knows the rate.
    pub units_per_hour: Option<f64>,
}

/// What an account has spent and is spending.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    pub account: String,
    pub balance_units: Option<f64>,
    /// Units per hour, as the app last read them. `None` when it has never read them, which is
    /// not the same as zero.
    pub burn_units_per_hour: Option<f64>,
    /// The numbers are the last known ones: a later reading failed.
    pub stale: bool,
}

/// A plugin run the app knows about.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub plugin: String,
    pub session: String,
    pub stage: String,
    /// Whether the run has said it is ready to serve.
    pub ready: bool,
    /// The tail of the run's log, oldest line first. Secrets are already redacted.
    pub log_tail: String,
    pub workflow: Workflow,
    pub outputs: Vec<Output>,
}

/// The workflow a run published: its nodes and the links between them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Workflow {
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    /// The id inside the workflow (ComfyUI's node id, for instance).
    pub id: String,
    /// The node's type, as the workflow names it.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Link {
    pub from: String,
    pub to: String,
}

/// A file a run produced, on the runtime.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Output {
    pub name: String,
    /// Its path on the runtime. Reading it here is slice 2's `run_download`.
    pub path: String,
}

/// An installed plugin.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Whether it ships with the app.
    pub bundled: bool,
}

/// An account signed in on this machine.
///
/// The address and which one is active. The token that signs the account in never has a field
/// here: an agent selects an account by this address and does not receive the credential.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Account {
    pub email: String,
    pub active: bool,
}

/// A plugin run the app is treating as a job (batch work that stops when it finishes).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Job {
    pub plugin: String,
    pub session: String,
    /// `running`, `ready`, `failed` or `cancelled`, as the run page shows the phase.
    pub state: String,
    /// The plugin's own stage id, or the phase name when it has not published one.
    pub stage: String,
}

/// How an external agent reaches a service this app has forwarded onto this computer.
///
/// `base_url` is a loopback HTTP address (`http://127.0.0.1:<port>/v1` for llama.cpp). A proxy
/// URL, an account token and an API key are not fields of this note.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Downstream {
    pub plugin: String,
    pub endpoint: String,
    pub kind: String,
    pub base_url: String,
    /// The model id the run published, or empty when it published none fit to show.
    pub model: String,
    /// The forward's health check passed. False until it has, including when no check has run.
    pub healthy: bool,
}

/// The account view an agent is allowed to see after it selects one by address.
///
/// `name` is the address it asked for. `accounts` is only address and active flag. A missing
/// active account is JSON null, the same as "none is active", not an empty address.
pub fn account_use_answer(name: &str, accounts: &[Account]) -> Result<Value> {
    let listed: Vec<Value> = accounts.iter().map(account_json).collect();
    let active = accounts
        .iter()
        .find(|account| account.active)
        .map(|account| account.email.clone());
    let value = json!({
        "name": name,
        "active": active,
        "accounts": listed,
    });
    ensure_public(&value)?;
    Ok(value)
}

fn account_json(account: &Account) -> Value {
    json!({ "email": account.email, "active": account.active })
}

/// Refuses a value whose key or text would hand an agent a credential.
///
/// The check is on the shape that is about to be returned. It does not look at the app's own
/// stores. A failure names the field and not the secret.
pub fn ensure_public(value: &Value) -> Result<()> {
    fn walk(value: &Value) -> Result<()> {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key_is_secret(key) {
                        return Err(Error::invalid(
                            key,
                            "a credential cannot be given to an agent",
                        ));
                    }
                    walk(child)?;
                }
                Ok(())
            }
            Value::Array(items) => {
                for item in items {
                    walk(item)?;
                }
                Ok(())
            }
            Value::String(text) if text_leaks(text) => Err(Error::invalid(
                "value",
                "a credential cannot be given to an agent",
            )),
            _ => Ok(()),
        }
    }
    walk(value)
}

fn key_is_secret(key: &str) -> bool {
    const PARTS: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "proxy",
        "credential",
        "authorization",
        "api_key",
        "oauth",
        "bearer",
        "cookie",
    ];
    let key = key.to_ascii_lowercase().replace('-', "_");
    PARTS
        .iter()
        .any(|part| key == *part || key.split('_').any(|piece| piece == *part))
}

/// Whether a string is carrying a credential rather than an address or a label.
pub fn text_leaks(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("proxy-token")
        || lower.contains("proxy_token")
        || lower.contains("access_token")
        || lower.contains("refresh_token")
        || lower.contains("bearer ")
        || lower.contains("authorization:")
        || lower.contains("colab-runtime-proxy-token")
}

/// `http://127.0.0.1:<port>/<path>` with no user, query or fragment.
///
/// Anything else is not a connection note this app will hand to an agent: a public host, a proxy
/// URL, or a token tucked into the query.
pub fn loopback_http(url: &str) -> bool {
    if text_leaks(url) || url.contains('@') || url.contains('?') || url.contains('#') {
        return false;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    if rest.contains("://") {
        return false;
    }
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return false;
    }
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    host == "127.0.0.1" && matches!(port.parse::<u16>(), Ok(port) if port > 0)
}

/// A model id published by a run, safe to repeat. A URL, a query or a credential is not one.
pub fn model_id_is_public(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && !model.contains("://")
        && !model.contains('?')
        && !model.contains('@')
        && !text_leaks(model)
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// A port forward through a session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forward {
    pub session: String,
    pub local_port: u16,
    pub remote_port: u16,
}

/// The settings an agent may read (and, in slice 2, write).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AppSettings {
    /// The interface language, as the app stores it (`system`, `en-US`, ...).
    pub language: String,
    pub appearance: String,
    /// The hardware new sessions ask for.
    pub default_hardware: String,
    /// The auto-stop new sessions ask for, as the app shows it.
    pub default_auto_stop: String,
    /// The WSL distribution the CLI runs in, on Windows.
    pub wsl_distro: Option<String>,
    /// Where `colab` is, when it was set by hand.
    pub colab_path: Option<String>,
    /// Whether the app is running on demo data (nothing runs and nothing is billed).
    pub demo_data: bool,
    /// Whether a call that would open a runtime, or move its auto-stop, asks first.
    pub ask_before_spend: bool,
}

/// A source with nothing in it, for tests and for the moment before the app has a state.
#[derive(Debug, Default)]
pub struct Empty;

impl Source for Empty {
    fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_source_answers_with_empty_lists() {
        let snapshot = Empty.snapshot();
        assert!(snapshot.sessions.is_empty());
        assert!(snapshot.settings.default_hardware.is_empty());
    }

    #[test]
    fn a_snapshot_serializes_without_nulls_for_absent_values() {
        let snapshot = Snapshot {
            sessions: vec![Session {
                name: "run-1".to_owned(),
                account: "a@b.c".to_owned(),
                hardware: "T4".to_owned(),
                state: "running".to_owned(),
                auto_stop: "30 min".to_owned(),
                units_per_hour: Some(1.5),
            }],
            ..Snapshot::default()
        };
        let json = serde_json::to_value(&snapshot).expect("serializes");
        assert_eq!(json["sessions"][0]["units_per_hour"], 1.5);
        assert_eq!(json["sessions"][0]["name"], "run-1");
        assert!(json["settings"]["wsl_distro"].is_null());
    }

    #[test]
    fn an_account_answer_names_the_address_and_not_a_credential() {
        let answer = account_use_answer(
            "a@b.c",
            &[
                Account {
                    email: "a@b.c".to_owned(),
                    active: true,
                },
                Account {
                    email: "c@d.e".to_owned(),
                    active: false,
                },
            ],
        )
        .expect("public");
        assert_eq!(answer["name"], "a@b.c");
        assert_eq!(answer["active"], "a@b.c");
        assert_eq!(answer["accounts"][0]["email"], "a@b.c");
        assert_eq!(answer["accounts"][0]["active"], true);
        let text = answer.to_string();
        assert!(!text.contains("token"), "{text}");
        assert!(!text.contains("proxy"), "{text}");
        let leaked = json!({ "accounts": [{ "email": "a@b.c", "token": "ya29" }] });
        assert!(
            ensure_public(&leaked).is_err(),
            "a token field is not returned"
        );
    }

    #[test]
    fn a_connection_note_is_loopback_and_a_credential_in_it_is_not() {
        assert!(loopback_http("http://127.0.0.1:8000/v1"));
        assert!(!loopback_http(
            "http://127.0.0.1:8000/v1?colab-runtime-proxy-token=secret"
        ));
        assert!(!loopback_http("http://example.invalid:8000/v1"));
        assert!(!loopback_http("http://token@127.0.0.1:8000/v1"));
        assert!(model_id_is_public("org/Qwen3-0.6B"));
        assert!(!model_id_is_public("refresh_token=abc"));
        assert!(!model_id_is_public("http://127.0.0.1:9/v1"));
    }
}
