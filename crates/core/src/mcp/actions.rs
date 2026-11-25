//! What an agent may ask the app to do, and the one way it is asked.
//!
//! Slice 1 gave an agent everything it may *read*. This is the other half. An [`Action`] is a
//! request whose arguments have already been checked: a value inside one is a value the app may
//! act on, so the app's half of a tool call is a match over this enum and nothing else.
//!
//! Core carries the vocabulary and no more. It changes no state, spends nothing and never asks a
//! person: an [`Actions`] implementation does that, and the app's is what turns a call into the
//! same work a click starts, including the confirmation an action that spends money needs. Keeping
//! the split here is what lets the tool table, the argument checks and the refusals be tested
//! without a window, and what keeps a second implementation of "start a session" from growing
//! beside the app's own.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::{Error, ErrorKind, Result};

/// One thing an agent asked for.
///
/// An `Action` is built from a tool call whose arguments passed their checks, so it holds typed
/// values: a port is a port, a change is a change. The optional members are the ones the app has a
/// setting for — an agent that names none of them gets what the app's own form would have used.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Provision a runtime. Nothing here is required: without them the app's defaults are used,
    /// which is what the new-session form starts with.
    SessionStart {
        name: Option<String>,
        hardware: Option<String>,
        auto_stop_minutes: Option<u64>,
    },
    /// Stop a runtime and end its billing.
    SessionStop { session: String },
    /// Move a runtime's auto-stop. Zero minutes cancels it, as the window's own button does.
    SessionExtend { session: String, minutes: u64 },
    /// Deploy and start a plugin on a runtime.
    PluginRun {
        plugin: String,
        session: Option<String>,
        /// The plugin's own inputs, as `plugin_run` was given them. The plugin's manifest is what
        /// says whether they are usable, and only the app has it.
        inputs: Map<String, Value>,
    },
    /// Copy what a run produced down to this computer.
    RunDownload { plugin: String },
    /// Change the app's settings. Only the app knows which names it has and which values are
    /// legal, so that check happens there.
    SettingsSet { changes: Map<String, Value> },
    /// Make another account the active one.
    AccountsSwitch { account: String },
    /// Enable or disable an installed plugin.
    PluginEnable { plugin: String, enabled: bool },
    /// Install a plugin from a folder on this computer or a Git URL.
    PluginInstall { source: String },
    /// Open a forward through a session. Without `local_port` the app picks a free one.
    ForwardOpen {
        session: String,
        remote_port: u16,
        local_port: Option<u16>,
    },
    /// Close a forward, by the session and the port it listens on here.
    ForwardClose { session: String, local_port: u16 },
}

impl Action {
    /// The tool this action came from. An error says which call it answers, so a client with
    /// several in flight can tell them apart.
    pub fn name(&self) -> &'static str {
        match self {
            Action::SessionStart { .. } => "session_start",
            Action::SessionStop { .. } => "session_stop",
            Action::SessionExtend { .. } => "session_extend",
            Action::PluginRun { .. } => "plugin_run",
            Action::RunDownload { .. } => "run_download",
            Action::SettingsSet { .. } => "settings_set",
            Action::AccountsSwitch { .. } => "accounts_switch",
            Action::PluginEnable { .. } => "plugin_enable",
            Action::PluginInstall { .. } => "plugin_install",
            Action::ForwardOpen { .. } => "forward_open",
            Action::ForwardClose { .. } => "forward_close",
        }
    }

    /// Whether carrying this out costs money. The four that do are the ones the app asks about
    /// first: starting, stopping, extending and running all end in a runtime that bills.
    pub fn is_paid(&self) -> bool {
        matches!(
            self,
            Action::SessionStart { .. }
                | Action::SessionStop { .. }
                | Action::SessionExtend { .. }
                | Action::PluginRun { .. }
        )
    }

    /// What the action is about, in the app's own words, for a dialog that has to name its target
    /// to a person who is deciding whether to allow it.
    pub fn target(&self) -> String {
        match self {
            Action::SessionStart { hardware, .. } => hardware
                .clone()
                .unwrap_or_else(|| "the default hardware".to_owned()),
            Action::SessionStop { session } | Action::SessionExtend { session, .. } => {
                session.clone()
            }
            Action::PluginRun {
                plugin, session, ..
            } => match session {
                Some(session) => format!("{plugin} on {session}"),
                None => plugin.clone(),
            },
            Action::RunDownload { plugin } | Action::PluginEnable { plugin, .. } => plugin.clone(),
            Action::SettingsSet { changes } => {
                let names: Vec<&str> = changes.keys().map(String::as_str).collect();
                names.join(", ")
            }
            Action::AccountsSwitch { account } => account.clone(),
            Action::PluginInstall { source } => source.clone(),
            Action::ForwardOpen {
                session,
                remote_port,
                ..
            } => format!("{session}: {remote_port}"),
            Action::ForwardClose {
                session,
                local_port,
            } => format!("{session}: {local_port}"),
        }
    }
}

/// Carries out what an agent asked for.
///
/// The endpoint calls this on its own thread and holds the client's response open until it
/// returns, which is deliberate: an agent whose call is waiting for a person must not be answered
/// "queued" and left to poll. An implementation that cannot answer must return an error rather
/// than block — a [`crate::error::ErrorKind::Denied`] once a person has said no.
pub trait Actions: Send + Sync {
    /// Does what `action` asks, on behalf of the client named by `client` (the name the MCP
    /// client introduced itself with), and answers with what the tool reports.
    fn perform(&self, action: Action, client: &str) -> Result<Value>;
}

impl Actions for Arc<dyn Actions> {
    fn perform(&self, action: Action, client: &str) -> Result<Value> {
        (**self).perform(action, client)
    }
}

/// An executor that does nothing, for a test that only reads and for an endpoint that was started
/// without an app behind it.
///
/// It answers with [`ErrorKind::Unsupported`] rather than refusing like a person would: nothing
/// was declined, there is simply nothing here to do it.
#[derive(Debug, Default)]
pub struct Unavailable;

impl Actions for Unavailable {
    fn perform(&self, action: Action, _client: &str) -> Result<Value> {
        let name = action.name();
        Err(Error::new(ErrorKind::Unsupported)
            .with("action", name)
            .detail(format!("{name} is not carried out here")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// One of every action, so a rule about all of them can be checked over all of them.
    fn every_action() -> Vec<Action> {
        vec![
            Action::SessionStart {
                name: Some("run-1".to_owned()),
                hardware: Some("T4".to_owned()),
                auto_stop_minutes: Some(30),
            },
            Action::SessionStop {
                session: "run-1".to_owned(),
            },
            Action::SessionExtend {
                session: "run-1".to_owned(),
                minutes: 30,
            },
            Action::PluginRun {
                plugin: "comfyui".to_owned(),
                session: Some("run-1".to_owned()),
                inputs: Map::new(),
            },
            Action::RunDownload {
                plugin: "comfyui".to_owned(),
            },
            Action::SettingsSet {
                changes: Map::from_iter([("language".to_owned(), json!("en-US"))]),
            },
            Action::AccountsSwitch {
                account: "a@b.c".to_owned(),
            },
            Action::PluginEnable {
                plugin: "comfyui".to_owned(),
                enabled: false,
            },
            Action::PluginInstall {
                source: "https://example.invalid/p.git".to_owned(),
            },
            Action::ForwardOpen {
                session: "run-1".to_owned(),
                remote_port: 8188,
                local_port: None,
            },
            Action::ForwardClose {
                session: "run-1".to_owned(),
                local_port: 8188,
            },
        ]
    }

    #[test]
    fn every_action_names_the_tool_it_came_from() {
        let names: Vec<&str> = every_action().iter().map(Action::name).collect();
        assert_eq!(
            names,
            [
                "session_start",
                "session_stop",
                "session_extend",
                "plugin_run",
                "run_download",
                "settings_set",
                "accounts_switch",
                "plugin_enable",
                "plugin_install",
                "forward_open",
                "forward_close",
            ]
        );
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "two actions share a name");
    }

    #[test]
    fn only_the_four_that_leave_a_runtime_billing_are_paid() {
        let paid: Vec<&str> = every_action()
            .iter()
            .filter(|action| action.is_paid())
            .map(Action::name)
            .collect();
        assert_eq!(
            paid,
            [
                "session_start",
                "session_stop",
                "session_extend",
                "plugin_run"
            ],
            "an agent spending money is the thing the window has to ask about"
        );
    }

    #[test]
    fn an_action_says_what_it_is_about() {
        // The dialog names the target to a person, so an empty name is useless.
        for action in every_action() {
            assert!(!action.target().is_empty(), "{:?}", action.name());
        }
        let named = Action::SettingsSet {
            changes: Map::from_iter([
                ("language".to_owned(), json!("en-US")),
                ("appearance".to_owned(), json!("dark")),
            ]),
        }
        .target();
        assert!(
            named.contains("language") && named.contains("appearance"),
            "a settings change names every field it would move: {named}"
        );
        assert_eq!(
            Action::SessionStart {
                name: None,
                hardware: None,
                auto_stop_minutes: None
            }
            .target(),
            "the default hardware",
            "an agent that named no hardware gets the app's own answer"
        );
    }

    #[test]
    fn an_executor_that_does_nothing_refuses_rather_than_panicking() {
        let error = Unavailable
            .perform(
                Action::SessionStop {
                    session: "x".to_owned(),
                },
                "a client",
            )
            .expect_err("nothing is done here");
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(error.param("action"), Some("session_stop"));
    }
}
