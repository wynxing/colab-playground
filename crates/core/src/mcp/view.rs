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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Account {
    pub email: String,
    pub active: bool,
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
}
