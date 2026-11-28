//! The tools an agent may call, and the arguments each one accepts.
//!
//! A tool is one of two things. A **read** tool answers from [`Snapshot`](super::view::Snapshot)
//! and changes nothing — slice 1, and still the half an agent can use the moment the endpoint is
//! switched on. A **write** tool checks its arguments into an [`Action`] and hands it to the app's
//! [`Actions`]: core says what was asked for, and the app is the one that does it, asks the user
//! where money is involved, and says what happened.
//!
//! Descriptions are written for a model that has never seen this app: what the tool does, what it
//! answers, and what it does *not* do. A paid tool's description says the user is asked first,
//! because a caller that expects one answer and gets a refusal must not read it as a failure worth
//! retrying.

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};

use super::{
    actions::{Action, Actions},
    view::Snapshot,
};

/// One tool: its name, what it is for, the arguments it takes and what it does with them.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// The JSON Schema for the arguments. A function so the table stays free of `Value`s.
    pub input: fn() -> Value,
    pub kind: Kind,
    pub read_only: bool,
}

/// The two things a tool can do.
pub enum Kind {
    /// Answers from the snapshot.
    Read(fn(&Snapshot, &Args) -> Result<Value>),
    /// Checks its arguments into an action for the app to carry out.
    Write(fn(&Args) -> Result<Action>),
}

/// The arguments of a call, checked against what the tool accepts.
pub struct Args {
    object: Map<String, Value>,
}

impl Args {
    /// Parses the `arguments` of a `tools/call`. Absent arguments are an empty set, which is what
    /// a tool that takes none expects.
    pub fn parse(value: Option<&Value>) -> Result<Self> {
        match value {
            None | Some(Value::Null) => Ok(Args { object: Map::new() }),
            Some(Value::Object(object)) => Ok(Args {
                object: object.clone(),
            }),
            Some(_) => Err(Error::invalid("arguments", "not an object")),
        }
    }

    /// A required string argument.
    fn string(&self, name: &str) -> Result<&str> {
        match self.object.get(name) {
            Some(Value::String(text)) if !text.is_empty() => Ok(text),
            Some(_) => Err(Error::invalid(name, "not a non-empty string")),
            None => Err(Error::invalid(name, "missing")),
        }
    }

    /// A string argument that may be left out. An empty one is refused rather than read as
    /// "absent": a caller that meant to name something and sent nothing has a bug worth hearing
    /// about.
    fn maybe_string(&self, name: &str) -> Result<Option<&str>> {
        match self.object.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) if !text.is_empty() => Ok(Some(text)),
            Some(_) => Err(Error::invalid(name, "not a non-empty string")),
        }
    }

    /// A required `true` or `false`.
    fn flag(&self, name: &str) -> Result<bool> {
        match self.object.get(name) {
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(Error::invalid(name, "not true or false")),
            None => Err(Error::invalid(name, "missing")),
        }
    }

    /// A required whole number of zero or more.
    fn number(&self, name: &str) -> Result<u64> {
        match self.object.get(name) {
            Some(Value::Number(number)) => number
                .as_u64()
                .ok_or_else(|| Error::invalid(name, "not a whole number of zero or more")),
            Some(_) => Err(Error::invalid(name, "not a number")),
            None => Err(Error::invalid(name, "missing")),
        }
    }

    /// A whole number that may be left out.
    fn maybe_number(&self, name: &str) -> Result<Option<u64>> {
        match self.object.get(name) {
            None | Some(Value::Null) => Ok(None),
            _ => self.number(name).map(Some),
        }
    }

    /// A required TCP port.
    fn port(&self, name: &str) -> Result<u16> {
        self.number(name)?
            .try_into()
            .ok()
            .filter(|port: &u16| *port > 0)
            .ok_or_else(|| Error::invalid(name, "not a port between 1 and 65535"))
    }

    /// A port that may be left out.
    fn maybe_port(&self, name: &str) -> Result<Option<u16>> {
        match self.object.get(name) {
            None | Some(Value::Null) => Ok(None),
            _ => self.port(name).map(Some),
        }
    }

    /// A required object that is not empty, such as a set of settings or a plugin's inputs.
    fn object(&self, name: &str) -> Result<Map<String, Value>> {
        match self.object.get(name) {
            Some(Value::Object(object)) if !object.is_empty() => Ok(object.clone()),
            Some(Value::Object(_)) => Err(Error::invalid(name, "empty")),
            Some(_) => Err(Error::invalid(name, "not an object")),
            None => Err(Error::invalid(name, "missing")),
        }
    }

    /// An object that may be left out; an absent one is an empty set.
    fn maybe_object(&self, name: &str) -> Result<Map<String, Value>> {
        match self.object.get(name) {
            None | Some(Value::Null) => Ok(Map::new()),
            _ => self.object(name),
        }
    }

    /// Refuses an argument the tool does not take: a caller that sends one has misread the tool,
    /// and saying so is more useful than ignoring it.
    fn only(&self, allowed: &[&str]) -> Result<()> {
        for key in self.object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(Error::invalid(
                    key,
                    format!("this tool takes {}", describe(allowed)),
                ));
            }
        }
        Ok(())
    }
}

fn describe(allowed: &[&str]) -> String {
    if allowed.is_empty() {
        "no arguments".to_owned()
    } else {
        format!("only {}", allowed.join(", "))
    }
}

fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn string_property(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn plugin_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": string_property("The plugin id, as plugin_list answers it.")
        },
        "required": ["plugin"],
        "additionalProperties": false
    })
}

fn session_watch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": string_property(
                "Optional runtime name to watch, as sessions_list answers it. When absent, \
                 watches all active runtimes and forwards."
            )
        },
        "additionalProperties": false
    })
}

fn session_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": string_property("The runtime's name, as sessions_list answers it.")
        },
        "required": ["session"],
        "additionalProperties": false
    })
}

fn session_start_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": string_property(
                "What to call the runtime. The app makes one up when this is absent."
            ),
            "hardware": string_property(
                "The accelerator, as sessions_list names it (`T4`, `A100`, ...). The app's default \
                 is used when this is absent."
            ),
            "auto_stop_minutes": {
                "type": "integer",
                "minimum": 0,
                "description": "Minutes before the runtime stops itself. The app's default is used \
                                when this is absent."
            }
        },
        "additionalProperties": false
    })
}

fn session_extend_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": string_property("The runtime's name, as sessions_list answers it."),
            "minutes": {
                "type": "integer",
                "minimum": 0,
                "description": "Minutes from now until the runtime stops itself. Zero cancels the \
                                auto-stop, which leaves it billing until it is stopped by hand."
            }
        },
        "required": ["session", "minutes"],
        "additionalProperties": false
    })
}

fn plugin_run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": string_property("The plugin id, as plugin_list answers it."),
            "session": string_property(
                "The runtime to deploy it on. The run form's own choice is used when this is \
                 absent."
            ),
            "inputs": {
                "type": "object",
                "description": "The plugin's own inputs, by the names its manifest gives them. The \
                                app checks them and answers with the manifest's complaint when one \
                                is wrong."
            }
        },
        "required": ["plugin"],
        "additionalProperties": false
    })
}

fn settings_set_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "changes": {
                "type": "object",
                "description": "The fields to change, by the names settings_get reports, with \
                                their new values. The app checks each one the way its own form \
                                does. The agent endpoint's own switch and port are not among the \
                                fields an agent may change."
            }
        },
        "required": ["changes"],
        "additionalProperties": false
    })
}

fn accounts_switch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "account": string_property("The account's address, as accounts_list answers it.")
        },
        "required": ["account"],
        "additionalProperties": false
    })
}

fn account_use_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": string_property(
                "The account's address, as accounts_list reports it in email. Not a token."
            )
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

fn job_run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": string_property(
                "The plugin id, as plugin_list answers it. The plugin's run mode must be job."
            ),
            "session": string_property(
                "The runtime to run it on. The run form's own choice is used when this is absent, \
                 which opens a runtime."
            ),
            "inputs": {
                "type": "object",
                "description": "The plugin's own inputs, by the names its manifest gives them. A \
                                secret is not an argument: the app keeps those and refuses a call \
                                that sends one."
            }
        },
        "required": ["plugin"],
        "additionalProperties": false
    })
}

fn downstream_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": string_property(
                "The plugin id, as plugin_list answers it. Every forwarded service is included \
                 when this is absent."
            )
        },
        "additionalProperties": false
    })
}

fn plugin_enable_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": string_property("The plugin id, as plugin_list answers it."),
            "enabled": {
                "type": "boolean",
                "description": "True to enable it, false to disable it."
            }
        },
        "required": ["plugin", "enabled"],
        "additionalProperties": false
    })
}

fn plugin_install_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": string_property(
                "A folder on this computer holding the plugin, or a Git URL to clone it from."
            )
        },
        "required": ["source"],
        "additionalProperties": false
    })
}

fn forward_open_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": string_property("The runtime's name, as sessions_list answers it."),
            "remote_port": {
                "type": "integer",
                "minimum": 1,
                "maximum": 65535,
                "description": "The port on the runtime to reach."
            },
            "local_port": {
                "type": "integer",
                "minimum": 1,
                "maximum": 65535,
                "description": "The port to listen on here. A free one is picked when this is \
                                absent."
            }
        },
        "required": ["session", "remote_port"],
        "additionalProperties": false
    })
}

fn forward_close_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": string_property("The runtime the forward goes through."),
            "local_port": {
                "type": "integer",
                "minimum": 1,
                "maximum": 65535,
                "description": "The port it listens on here, as forwards_list reports it."
            }
        },
        "required": ["session", "local_port"],
        "additionalProperties": false
    })
}

/// The tools, in the order `tools/list` reports them: what an agent reads first, then what it can
/// set going.
pub fn all() -> &'static [Tool] {
    &[
        Tool {
            name: "sessions_list",
            description: "The Colab runtimes of every signed-in account: name, hardware, state, \
                          when each stops by itself and what it costs per hour. Read-only: nothing \
                          is started or stopped.",
            input: empty_schema,
            kind: Kind::Read(sessions_list),
            read_only: true,
        },
        Tool {
            name: "session_usage",
            description: "Each account's compute-unit balance and the rate it is burning at right \
                          now. Read-only.",
            input: empty_schema,
            kind: Kind::Read(session_usage),
            read_only: true,
        },
        Tool {
            name: "runs_list",
            description: "The plugin runs the app knows about: which plugin, on which session, and \
                          how far each has got. Read-only.",
            input: empty_schema,
            kind: Kind::Read(runs_list),
            read_only: true,
        },
        Tool {
            name: "run_status",
            description: "One plugin run: its stage, whether it has said it is ready, and the tail \
                          of its log (secrets already redacted). Read-only.",
            input: plugin_schema,
            kind: Kind::Read(run_status),
            read_only: true,
        },
        Tool {
            name: "run_watch",
            description: "Waits until the run changes — a new stage, ready, failed, or the runtime \
                          gone — and answers with the run as it is then. It answers anyway after \
                          about two minutes with whatever it is still doing, so a long job is a \
                          series of short calls rather than one call a client may abandon. Call it \
                          again to keep watching. It changes nothing.",
            input: plugin_schema,
            kind: Kind::Write(run_watch),
            read_only: true,
        },
        Tool {
            name: "session_watch",
            description: "Waits until a runtime raises a cost-guard warning, approaches its \
                          auto-stop deadline, stops or vanishes, or a port forward disconnects or \
                          becomes unhealthy — and answers with the sessions, warnings and forwards \
                          as they stand then. Answers anyway after about two minutes so the call \
                          never hits a client timeout. Call it again to keep watching. It changes \
                          nothing.",
            input: session_watch_schema,
            kind: Kind::Write(session_watch),
            read_only: true,
        },
        Tool {
            name: "run_outputs",
            description: "What one plugin run published: the workflow's nodes and links, and the \
                          paths of the files it produced on the runtime. Read-only; run_download \
                          is what copies them here.",
            input: plugin_schema,
            kind: Kind::Read(run_outputs),
            read_only: true,
        },
        Tool {
            name: "plugin_list",
            description: "The plugins installed in this app: id, name, whether each is enabled, and \
                          whether it ships with the app. Read-only.",
            input: empty_schema,
            kind: Kind::Read(plugin_list),
            read_only: true,
        },
        Tool {
            name: "settings_get",
            description: "The app's settings: language, appearance, the hardware and auto-stop new \
                          sessions ask for, host details, and whether spending asks first. \
                          Read-only.",
            input: empty_schema,
            kind: Kind::Read(settings_get),
            read_only: true,
        },
        Tool {
            name: "accounts_list",
            description: "The Google accounts signed in on this machine and which one is active. \
                          Read-only: each entry is the address and the active flag, never a token. \
                          account_use is what selects one.",
            input: empty_schema,
            kind: Kind::Read(accounts_list),
            read_only: true,
        },
        Tool {
            name: "job_status",
            description: "One job the app is running or has just finished: its plugin, session, \
                          stage and state. Read-only. A plugin that is not a job is not here.",
            input: plugin_schema,
            kind: Kind::Read(job_status),
            read_only: true,
        },
        Tool {
            name: "downstream_config",
            description: "A read-only connection note for a service this app has forwarded onto \
                          this computer, such as a llama.cpp API at http://127.0.0.1:<port>/v1. \
                          It names that local address, the model id when the run published one, \
                          and whether the health check passed. It contains no token, proxy address \
                          or API key. An empty list means nothing is forwarded yet. Connect only \
                          when healthy is true.",
            input: downstream_schema,
            kind: Kind::Read(downstream_config),
            read_only: true,
        },
        Tool {
            name: "forwards_list",
            description: "The port forwards that are open through sessions, with the local port an \
                          agent would connect to. Read-only.",
            input: empty_schema,
            kind: Kind::Read(forwards_list),
            read_only: true,
        },
        Tool {
            name: "session_start",
            description: "Starts a Colab runtime, which begins billing. While asking is switched \
                          on, the app asks the user in its own window first and holds this call \
                          open until they answer; being refused is final, so do not call again. \
                          Answers when the runtime exists, with what it costs per hour.",
            input: session_start_schema,
            kind: Kind::Write(session_start),
            read_only: false,
        },
        Tool {
            name: "session_stop",
            description: "Stops a runtime and ends its billing. The app does this at once and does \
                          not ask: stopping does not open a new charge.",
            input: session_schema,
            kind: Kind::Write(session_stop),
            read_only: false,
        },
        Tool {
            name: "session_extend",
            description: "Moves a runtime's auto-stop, or cancels it with zero minutes (after \
                          which it bills until it is stopped by hand). While asking is switched \
                          on, the app asks the user in its own window first and holds this call \
                          open until they answer; being refused is final, so do not call again.",
            input: session_extend_schema,
            kind: Kind::Write(session_extend),
            read_only: false,
        },
        Tool {
            name: "plugin_run",
            description: "Deploys and starts a plugin on a runtime, with the inputs its manifest \
                          declares. A call that names a session that is already open starts at \
                          once and does not ask. A call that leaves the session out opens a new \
                          runtime: while asking is switched on, the app asks the user in its own \
                          window first and holds this call open until they answer; being refused \
                          is final, so do not call again. Answers when the run has reached a \
                          stage, not when it finishes, so read run_status while it works.",
            input: plugin_run_schema,
            kind: Kind::Write(plugin_run),
            read_only: false,
        },
        Tool {
            name: "run_download",
            description: "Copies the files a run produced from the runtime to this computer, into \
                          the app's own downloads folder. Answers once they are written, with \
                          their paths here.",
            input: plugin_schema,
            kind: Kind::Write(run_download),
            read_only: false,
        },
        Tool {
            name: "settings_set",
            description: "Changes the app's settings, among the fields settings_get reports. Each \
                          value is checked the way the app's own form checks it; the endpoint's \
                          own switch and port are not among them, and neither is whether spending \
                          asks first.",
            input: settings_set_schema,
            kind: Kind::Write(settings_set),
            read_only: false,
        },
        Tool {
            name: "accounts_switch",
            description: "Makes another signed-in account the active one, which is the account a \
                          session is then started on. Answers when the switch is done.",
            input: accounts_switch_schema,
            kind: Kind::Write(accounts_switch),
            read_only: false,
        },
        Tool {
            name: "plugin_enable",
            description: "Enables or disables an installed plugin. Answers when the change is \
                          stored, with the plugin list as it then stands.",
            input: plugin_enable_schema,
            kind: Kind::Write(plugin_enable),
            read_only: false,
        },
        Tool {
            name: "plugin_install",
            description: "Installs a plugin from a folder on this computer or a Git URL. The app \
                          checks the manifest before it installs anything, and answers with the \
                          manifest's own complaint when it cannot.",
            input: plugin_install_schema,
            kind: Kind::Write(plugin_install),
            read_only: false,
        },
        Tool {
            name: "forward_open",
            description: "Opens a port forward through a session, so something on this computer \
                          can reach a port on the runtime. Answers when it is up, with the local \
                          port to connect to.",
            input: forward_open_schema,
            kind: Kind::Write(forward_open),
            read_only: false,
        },
        Tool {
            name: "forward_close",
            description: "Closes a port forward that is open. Answers when it is gone.",
            input: forward_close_schema,
            kind: Kind::Write(forward_close),
            read_only: false,
        },
        Tool {
            name: "account_use",
            description: "Selects a signed-in account by the address accounts_list reports, and \
                          makes it the active one for what runs next. The account's token stays \
                          in the app: this tool neither takes it nor returns it. Selecting an \
                          account does not itself start billing.",
            input: account_use_schema,
            kind: Kind::Write(account_use),
            read_only: false,
        },
        Tool {
            name: "job_run",
            description: "Starts a plugin whose run mode is job, on the app's existing job \
                          lifecycle (it persists what the plugin keeps, then stops a runtime this \
                          run opened). While asking is switched on, the app asks the user in its \
                          own window first — even when the session is already open, because \
                          starting a job bills — and holds this call open until they answer; \
                          being refused is final, so do not call again. Do not send a token or a \
                          secret: the app keeps those, and a call that includes one is refused. \
                          Answers when the job has reached a stage, not when it finishes.",
            input: job_run_schema,
            kind: Kind::Write(job_run),
            read_only: false,
        },
        Tool {
            name: "job_cancel",
            description: "Stops a job the app is running. The app does this at once and does not \
                          ask: cancelling does not open a new charge. A plugin that is not a job, \
                          or a job that is not running, is refused.",
            input: plugin_schema,
            kind: Kind::Write(job_cancel),
            read_only: false,
        },
    ]
}

/// One tool by name.
pub fn find(name: &str) -> Option<&'static Tool> {
    all().iter().find(|tool| tool.name == name)
}

/// The `tools/list` result.
pub fn descriptors() -> Value {
    let tools: Vec<Value> = all()
        .iter()
        .map(|tool| {
            let mut entry = json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": (tool.input)(),
            });
            if tool.read_only {
                entry["annotations"] = json!({ "readOnlyHint": true });
            }
            entry
        })
        .collect();
    json!({ "tools": tools })
}

/// Runs one tool, for the client named by `client`.
///
/// An unknown name is an error the caller reports as a JSON-RPC fault; a tool that refuses its
/// arguments and an action the app refuses are both reported as tool results — the client did
/// reach the tool either way.
pub fn call(
    snapshot: &Snapshot,
    actions: &dyn Actions,
    name: &str,
    arguments: Option<&Value>,
    client: &str,
) -> Result<Value> {
    let tool =
        find(name).ok_or_else(|| Error::invalid("name", format!("no tool called {name}")))?;
    let args = Args::parse(arguments)?;
    match tool.kind {
        Kind::Read(read) => Ok(with_notes(snapshot, read(snapshot, &args)?)),
        // What a write tool answers is the app's, and it carries no notes: the app has just said
        // what it did, and a listing a minute old is not that answer's business.
        Kind::Write(write) => actions.perform(write(&args)?, client),
    }
}

/// Adds what the app could not tell to an answer that has any. An empty list and "the app has not
/// looked" are different facts, and only one of them means nothing is running.
fn with_notes(snapshot: &Snapshot, mut answer: Value) -> Value {
    if !snapshot.notes.is_empty()
        && let Some(object) = answer.as_object_mut()
    {
        object.insert("notes".to_owned(), json!(snapshot.notes));
    }
    answer
}

fn sessions_list(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    Ok(json!({ "sessions": snapshot.sessions }))
}

fn session_usage(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    Ok(json!({ "usage": snapshot.usage }))
}

fn runs_list(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    let runs: Vec<Value> = snapshot
        .runs
        .iter()
        .map(|run| {
            json!({
                "plugin": run.plugin,
                "session": run.session,
                "stage": run.stage,
                "ready": run.ready,
            })
        })
        .collect();
    Ok(json!({ "runs": runs }))
}

/// The run of one plugin, by the name the caller used.
fn run_of<'a>(snapshot: &'a Snapshot, args: &Args) -> Result<(&'a super::view::Run, String)> {
    args.only(&["plugin"])?;
    let plugin = args.string("plugin")?;
    let run = snapshot
        .runs
        .iter()
        .find(|run| run.plugin == plugin)
        .ok_or_else(|| Error::invalid("plugin", format!("no run of {plugin}: see runs_list")))?;
    Ok((run, plugin.to_owned()))
}

fn run_status(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    let (run, plugin) = run_of(snapshot, args)?;
    Ok(json!({
        "plugin": plugin,
        "session": run.session,
        "stage": run.stage,
        "ready": run.ready,
        "log_tail": run.log_tail,
    }))
}

fn run_outputs(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    let (run, plugin) = run_of(snapshot, args)?;
    Ok(json!({
        "plugin": plugin,
        "workflow": run.workflow,
        "outputs": run.outputs,
    }))
}

fn plugin_list(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    Ok(json!({ "plugins": snapshot.plugins }))
}

fn settings_get(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    Ok(json!({ "settings": snapshot.settings }))
}

fn accounts_list(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    let accounts: Vec<Value> = snapshot
        .accounts
        .iter()
        .map(|account| json!({ "email": account.email, "active": account.active }))
        .collect();
    let value = json!({ "accounts": accounts });
    super::view::ensure_public(&value)?;
    Ok(value)
}

fn job_status(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&["plugin"])?;
    let plugin = args.string("plugin")?;
    let job = snapshot
        .jobs
        .iter()
        .find(|job| job.plugin == plugin)
        .ok_or_else(|| Error::invalid("plugin", format!("no job of {plugin}")))?;
    let value = json!({
        "plugin": job.plugin,
        "session": job.session,
        "state": job.state,
        "stage": job.stage,
    });
    super::view::ensure_public(&value)?;
    Ok(value)
}

fn downstream_config(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&["plugin"])?;
    let plugin = args.maybe_string("plugin")?;
    let tools: Vec<Value> = snapshot
        .downstream
        .iter()
        .filter(|item| plugin.is_none_or(|name| item.plugin == name))
        .filter(|item| super::view::loopback_http(&item.base_url))
        .map(|item| {
            let model = if super::view::model_id_is_public(&item.model) {
                item.model.as_str()
            } else {
                ""
            };
            json!({
                "plugin": item.plugin,
                "endpoint": item.endpoint,
                "kind": item.kind,
                "base_url": item.base_url,
                "model": model,
                "healthy": item.healthy,
            })
        })
        .collect();
    let value = json!({ "tools": tools });
    super::view::ensure_public(&value)?;
    Ok(value)
}

fn forwards_list(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    Ok(json!({ "forwards": snapshot.forwards }))
}

fn session_start(args: &Args) -> Result<Action> {
    args.only(&["name", "hardware", "auto_stop_minutes"])?;
    Ok(Action::SessionStart {
        name: args.maybe_string("name")?.map(str::to_owned),
        hardware: args.maybe_string("hardware")?.map(str::to_owned),
        auto_stop_minutes: args.maybe_number("auto_stop_minutes")?,
    })
}

fn session_stop(args: &Args) -> Result<Action> {
    args.only(&["session"])?;
    Ok(Action::SessionStop {
        session: args.string("session")?.to_owned(),
    })
}

fn session_extend(args: &Args) -> Result<Action> {
    args.only(&["session", "minutes"])?;
    Ok(Action::SessionExtend {
        session: args.string("session")?.to_owned(),
        minutes: args.number("minutes")?,
    })
}

fn plugin_run(args: &Args) -> Result<Action> {
    args.only(&["plugin", "session", "inputs"])?;
    Ok(Action::PluginRun {
        plugin: args.string("plugin")?.to_owned(),
        session: args.maybe_string("session")?.map(str::to_owned),
        inputs: args.maybe_object("inputs")?,
    })
}

fn run_watch(args: &Args) -> Result<Action> {
    args.only(&["plugin"])?;
    Ok(Action::RunWatch {
        plugin: args.string("plugin")?.to_owned(),
    })
}

fn session_watch(args: &Args) -> Result<Action> {
    args.only(&["session"])?;
    Ok(Action::SessionWatch {
        session: args.maybe_string("session")?.map(str::to_owned),
    })
}

fn run_download(args: &Args) -> Result<Action> {
    args.only(&["plugin"])?;
    Ok(Action::RunDownload {
        plugin: args.string("plugin")?.to_owned(),
    })
}

fn settings_set(args: &Args) -> Result<Action> {
    args.only(&["changes"])?;
    Ok(Action::SettingsSet {
        changes: args.object("changes")?,
    })
}

fn accounts_switch(args: &Args) -> Result<Action> {
    args.only(&["account"])?;
    Ok(Action::AccountsSwitch {
        account: args.string("account")?.to_owned(),
    })
}

/// An account address, not a credential. One `@`, a dotted host, no scheme and no spaces.
fn account_name(value: &str) -> Result<&str> {
    if value.contains("://")
        || value.contains(char::is_whitespace)
        || value.contains('/')
        || value.matches('@').count() != 1
    {
        return Err(Error::invalid("name", "not an account address"));
    }
    let (local, host) = value
        .split_once('@')
        .ok_or_else(|| Error::invalid("name", "not an account address"))?;
    if local.is_empty() || !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
        return Err(Error::invalid("name", "not an account address"));
    }
    Ok(value)
}

fn account_use(args: &Args) -> Result<Action> {
    args.only(&["name"])?;
    Ok(Action::AccountUse {
        name: account_name(args.string("name")?)?.to_owned(),
    })
}

fn job_run(args: &Args) -> Result<Action> {
    args.only(&["plugin", "session", "inputs"])?;
    Ok(Action::JobRun {
        plugin: args.string("plugin")?.to_owned(),
        session: args.maybe_string("session")?.map(str::to_owned),
        inputs: args.maybe_object("inputs")?,
    })
}

fn job_cancel(args: &Args) -> Result<Action> {
    args.only(&["plugin"])?;
    Ok(Action::JobCancel {
        plugin: args.string("plugin")?.to_owned(),
    })
}

fn plugin_enable(args: &Args) -> Result<Action> {
    args.only(&["plugin", "enabled"])?;
    Ok(Action::PluginEnable {
        plugin: args.string("plugin")?.to_owned(),
        enabled: args.flag("enabled")?,
    })
}

fn plugin_install(args: &Args) -> Result<Action> {
    args.only(&["source"])?;
    Ok(Action::PluginInstall {
        source: args.string("source")?.to_owned(),
    })
}

fn forward_open(args: &Args) -> Result<Action> {
    args.only(&["session", "remote_port", "local_port"])?;
    Ok(Action::ForwardOpen {
        session: args.string("session")?.to_owned(),
        remote_port: args.port("remote_port")?,
        local_port: args.maybe_port("local_port")?,
    })
}

fn forward_close(args: &Args) -> Result<Action> {
    args.only(&["session", "local_port"])?;
    Ok(Action::ForwardClose {
        session: args.string("session")?.to_owned(),
        local_port: args.port("local_port")?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{
        error::{Denial, ErrorKind},
        mcp::view::{Account, Downstream, Job, Output, Plugin, Run, Session, Workflow},
    };

    /// An executor that records what it was asked, and answers the way the app would.
    #[derive(Default)]
    struct Probe {
        asked: Mutex<Vec<(Action, String)>>,
        /// Refuse everything, the way a person who says no does.
        refusing: bool,
    }

    impl Probe {
        fn refusing() -> Self {
            Probe {
                refusing: true,
                ..Probe::default()
            }
        }

        fn asked(&self) -> Vec<(Action, String)> {
            self.asked.lock().expect("not poisoned").clone()
        }
    }

    impl Actions for Probe {
        fn perform(&self, action: Action, client: &str) -> Result<Value> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push((action.clone(), client.to_owned()));
            if self.refusing {
                return Err(Error::denied(action.name(), Denial::Refused));
            }
            Ok(json!({ "done": action.name() }))
        }
    }

    fn sample() -> Snapshot {
        Snapshot {
            sessions: vec![Session {
                name: "run-1".to_owned(),
                account: "a@b.c".to_owned(),
                hardware: "T4".to_owned(),
                state: "running".to_owned(),
                auto_stop: "30 min".to_owned(),
                units_per_hour: Some(1.5),
            }],
            runs: vec![Run {
                plugin: "comfyui".to_owned(),
                session: "run-1".to_owned(),
                stage: "ready".to_owned(),
                ready: true,
                log_tail: "serving on 8188".to_owned(),
                workflow: Workflow::default(),
                outputs: vec![Output {
                    name: "image.png".to_owned(),
                    path: "/content/ComfyUI/output/image.png".to_owned(),
                }],
            }],
            plugins: vec![Plugin {
                id: "comfyui".to_owned(),
                name: "ComfyUI".to_owned(),
                enabled: true,
                bundled: true,
            }],
            jobs: vec![Job {
                plugin: "batch".to_owned(),
                session: "run-1".to_owned(),
                state: "running".to_owned(),
                stage: "train".to_owned(),
            }],
            ..Snapshot::default()
        }
    }

    /// Arguments every tool accepts, so a rule can be checked over the whole table.
    fn arguments(name: &str) -> Option<Value> {
        Some(match name {
            "run_status" | "run_outputs" | "run_download" | "run_watch" => {
                json!({ "plugin": "comfyui" })
            }
            "session_watch" => json!({ "session": "run-1" }),
            "plugin_enable" => json!({ "plugin": "comfyui", "enabled": false }),
            "plugin_install" => json!({ "source": "/home/someone/plugins/comfyui" }),
            "plugin_run" => {
                json!({ "plugin": "comfyui", "session": "run-1", "inputs": { "quant": "auto" } })
            }
            "accounts_switch" => json!({ "account": "a@b.c" }),
            "account_use" => json!({ "name": "a@b.c" }),
            "job_run" => {
                json!({ "plugin": "batch", "session": "run-1", "inputs": { "path": "job.ipynb" } })
            }
            "job_cancel" | "job_status" => json!({ "plugin": "batch" }),
            "settings_set" => json!({ "changes": { "language": "en-US" } }),
            "session_start" => {
                json!({ "name": "run-2", "hardware": "T4", "auto_stop_minutes": 30 })
            }
            "session_stop" => json!({ "session": "run-1" }),
            "session_extend" => json!({ "session": "run-1", "minutes": 30 }),
            "forward_open" => {
                json!({ "session": "run-1", "remote_port": 8188, "local_port": 8188 })
            }
            "forward_close" => json!({ "session": "run-1", "local_port": 8188 }),
            _ => return None,
        })
    }

    /// What each write tool's arguments come to.
    fn expected(name: &str) -> Action {
        match name {
            "run_watch" => Action::RunWatch {
                plugin: "comfyui".to_owned(),
            },
            "session_watch" => Action::SessionWatch {
                session: Some("run-1".to_owned()),
            },
            "session_start" => Action::SessionStart {
                name: Some("run-2".to_owned()),
                hardware: Some("T4".to_owned()),
                auto_stop_minutes: Some(30),
            },
            "session_stop" => Action::SessionStop {
                session: "run-1".to_owned(),
            },
            "session_extend" => Action::SessionExtend {
                session: "run-1".to_owned(),
                minutes: 30,
            },
            "plugin_run" => Action::PluginRun {
                plugin: "comfyui".to_owned(),
                session: Some("run-1".to_owned()),
                inputs: Map::from_iter([("quant".to_owned(), json!("auto"))]),
            },
            "run_download" => Action::RunDownload {
                plugin: "comfyui".to_owned(),
            },
            "settings_set" => Action::SettingsSet {
                changes: Map::from_iter([("language".to_owned(), json!("en-US"))]),
            },
            "accounts_switch" => Action::AccountsSwitch {
                account: "a@b.c".to_owned(),
            },
            "account_use" => Action::AccountUse {
                name: "a@b.c".to_owned(),
            },
            "job_run" => Action::JobRun {
                plugin: "batch".to_owned(),
                session: Some("run-1".to_owned()),
                inputs: Map::from_iter([("path".to_owned(), json!("job.ipynb"))]),
            },
            "job_cancel" => Action::JobCancel {
                plugin: "batch".to_owned(),
            },
            "plugin_enable" => Action::PluginEnable {
                plugin: "comfyui".to_owned(),
                enabled: false,
            },
            "plugin_install" => Action::PluginInstall {
                source: "/home/someone/plugins/comfyui".to_owned(),
            },
            "forward_open" => Action::ForwardOpen {
                session: "run-1".to_owned(),
                remote_port: 8188,
                local_port: Some(8188),
            },
            "forward_close" => Action::ForwardClose {
                session: "run-1".to_owned(),
                local_port: 8188,
            },
            other => panic!("no expectation for {other}"),
        }
    }

    #[test]
    fn every_tool_is_named_uniquely_and_explains_itself() {
        let mut names: Vec<&str> = all().iter().map(|tool| tool.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two tools share a name");
        let reads = all()
            .iter()
            .filter(|tool| matches!(tool.kind, Kind::Read(_)))
            .count();
        assert_eq!(
            reads, 11,
            "slice 1's nine reads, plus job_status and downstream_config"
        );
        assert_eq!(
            count - reads,
            16,
            "the tools that ask the app to do something"
        );
        for tool in all() {
            assert!(tool.name.contains('_'), "{}: snake_case", tool.name);
            assert!(
                tool.description.len() > 40,
                "{}: the description is what an agent reads",
                tool.name
            );
            let schema = (tool.input)();
            assert_eq!(schema["type"], "object", "{}", tool.name);
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
            assert!(schema["properties"].is_object(), "{}", tool.name);
        }
    }

    #[test]
    fn tools_list_reports_what_the_table_holds() {
        let listed = descriptors();
        assert_eq!(listed["tools"].as_array().expect("tools").len(), 27);
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert!(names.contains(&"sessions_list"));
        assert!(names.contains(&"run_outputs"));
        assert!(names.contains(&"session_start"));
        assert!(names.contains(&"session_watch"));
    }

    #[test]
    fn run_watch_is_advertised_as_read_only() {
        let descriptors = descriptors();
        let watch = descriptors["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "run_watch")
            .unwrap();
        assert_eq!(watch["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn session_watch_is_advertised_as_read_only() {
        let descriptors = descriptors();
        let watch = descriptors["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "session_watch")
            .expect("session_watch descriptor");
        assert_eq!(watch["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn a_read_tool_answers_from_the_snapshot() {
        let snapshot = sample();
        let probe = Probe::default();
        let listed = call(&snapshot, &probe, "sessions_list", None, "a client").expect("answers");
        assert_eq!(listed["sessions"][0]["hardware"], "T4");
        let status = call(
            &snapshot,
            &probe,
            "run_status",
            Some(&json!({ "plugin": "comfyui" })),
            "a client",
        )
        .expect("answers");
        assert_eq!(status["stage"], "ready");
        assert_eq!(status["ready"], true);
        let outputs = call(
            &snapshot,
            &probe,
            "run_outputs",
            Some(&json!({ "plugin": "comfyui" })),
            "a client",
        )
        .expect("answers");
        assert_eq!(
            outputs["outputs"][0]["path"],
            "/content/ComfyUI/output/image.png"
        );
    }

    #[test]
    fn an_empty_state_answers_with_empty_lists_not_an_error() {
        let probe = Probe::default();
        for (name, key) in [
            ("sessions_list", "sessions"),
            ("session_usage", "usage"),
            ("runs_list", "runs"),
            ("plugin_list", "plugins"),
            ("accounts_list", "accounts"),
            ("forwards_list", "forwards"),
        ] {
            let answer = call(&Snapshot::default(), &probe, name, None, "a client").expect(name);
            assert!(answer[key].as_array().expect(name).is_empty(), "{name}");
        }
    }

    #[test]
    fn an_unknown_tool_names_itself_in_the_error() {
        let error = call(
            &Snapshot::default(),
            &Probe::default(),
            "session_launch",
            None,
            "a client",
        )
        .expect_err("unknown");
        assert_eq!(error.kind, ErrorKind::InvalidInput);
        assert!(error.detail.contains("session_launch"), "{}", error.detail);
    }

    #[test]
    fn a_run_that_does_not_exist_is_an_error_not_an_empty_answer() {
        let error = call(
            &Snapshot::default(),
            &Probe::default(),
            "run_status",
            Some(&json!({ "plugin": "comfyui" })),
            "a client",
        )
        .expect_err("no such run");
        assert!(error.detail.contains("runs_list"), "{}", error.detail);
    }

    #[test]
    fn arguments_are_checked_strictly() {
        let snapshot = sample();
        let probe = Probe::default();
        // A missing required argument.
        assert!(call(&snapshot, &probe, "run_status", Some(&json!({})), "c").is_err());
        // An argument of the wrong type.
        assert!(
            call(
                &snapshot,
                &probe,
                "run_status",
                Some(&json!({ "plugin": 7 })),
                "c"
            )
            .is_err()
        );
        // An argument this tool does not take.
        assert!(
            call(
                &snapshot,
                &probe,
                "sessions_list",
                Some(&json!({ "all": true })),
                "c"
            )
            .is_err(),
            "a caller that misreads the tool is told"
        );
        // Arguments that are not an object at all.
        assert!(
            call(
                &snapshot,
                &probe,
                "sessions_list",
                Some(&json!([1, 2])),
                "c"
            )
            .is_err()
        );
    }

    #[test]
    fn a_write_tool_checks_its_arguments_into_an_action() {
        for tool in all() {
            let Kind::Write(write) = tool.kind else {
                continue;
            };
            let Some(arguments) = arguments(tool.name) else {
                panic!("{}: no arguments to try", tool.name);
            };
            let args = Args::parse(Some(&arguments)).expect(tool.name);
            let action = write(&args)
                .unwrap_or_else(|error| panic!("{} refused its own arguments: {error}", tool.name));
            let expected = expected(tool.name);
            assert_eq!(action, expected, "{}", tool.name);
            assert_eq!(
                action.name(),
                tool.name,
                "an action names the tool it came from"
            );
        }
    }

    #[test]
    fn a_description_says_when_the_window_asks() {
        // The description is the only thing an agent reads. A call that always asks says so, a
        // call that never asks does not, and plugin_run says both because it depends on whether
        // a session was named.
        let text = |name: &str| {
            all()
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} is not a tool"))
                .description
                .to_owned()
        };
        for name in ["session_start", "session_extend", "plugin_run", "job_run"] {
            assert!(
                text(name).contains("asks the user in its own window"),
                "{name} must say the user is asked"
            );
        }
        for tool in all() {
            if matches!(
                tool.name,
                "session_start" | "session_extend" | "plugin_run" | "job_run"
            ) {
                continue;
            }
            assert!(
                !tool.description.contains("asks the user in its own window"),
                "{} must not say the user is asked",
                tool.name
            );
        }
        assert!(
            text("plugin_run").contains("already open"),
            "a plugin on a runtime that is already open does not ask"
        );
        assert!(
            text("session_stop").contains("does not ask"),
            "stopping is the call that saves money"
        );
        assert!(expected("session_start").needs_approval());
        assert!(expected("session_extend").needs_approval());
        assert!(!expected("session_stop").needs_approval());
        assert!(
            !expected("plugin_run").needs_approval(),
            "the sample plugin_run names a session, so it does not ask"
        );
        assert!(
            expected("job_run").needs_approval(),
            "the sample job_run names a session and still asks, because a job bills"
        );
        assert!(!expected("job_cancel").needs_approval());
        assert!(!expected("account_use").needs_approval());
    }

    #[test]
    fn a_write_tool_refuses_bad_arguments_before_anything_happens() {
        let probe = Probe::default();
        let snapshot = Snapshot::default();
        for (name, bad) in [
            ("session_stop", json!({})),
            ("session_stop", json!({ "session": 7 })),
            ("session_stop", json!({ "session": "" })),
            ("session_extend", json!({ "session": "run-1" })),
            (
                "session_extend",
                json!({ "session": "run-1", "minutes": -5 }),
            ),
            (
                "forward_open",
                json!({ "session": "run-1", "remote_port": 0 }),
            ),
            (
                "forward_open",
                json!({ "session": "run-1", "remote_port": 8188, "port": 1 }),
            ),
            ("settings_set", json!({ "changes": {} })),
            ("settings_set", json!({ "changes": [1, 2] })),
            (
                "plugin_enable",
                json!({ "plugin": "comfyui", "enabled": "yes" }),
            ),
            ("session_start", json!({ "auto_stop_minutes": "30" })),
            ("account_use", json!({})),
            ("account_use", json!({ "name": "" })),
            ("account_use", json!({ "name": "ya29.secret-token" })),
            (
                "account_use",
                json!({ "name": "http://token@127.0.0.1/v1" }),
            ),
            ("job_run", json!({})),
            ("job_cancel", json!({})),
            ("job_run", json!({ "plugin": "batch", "inputs": [] })),
        ] {
            let error = call(&snapshot, &probe, name, Some(&bad), "a client")
                .expect_err("refused before anything was asked");
            assert_eq!(error.kind, ErrorKind::InvalidInput, "{name} {bad}");
        }
        assert!(
            probe.asked().is_empty(),
            "a call with bad arguments never reaches the app"
        );
    }

    #[test]
    fn a_write_tool_goes_to_the_executor_and_a_read_tool_does_not() {
        let probe = Probe::default();
        let snapshot = sample();
        let answer = call(
            &snapshot,
            &probe,
            "session_stop",
            Some(&json!({ "session": "run-1" })),
            "an agent",
        )
        .expect("answers");
        assert_eq!(answer["done"], "session_stop");
        assert_eq!(
            probe.asked(),
            [(
                Action::SessionStop {
                    session: "run-1".to_owned()
                },
                "an agent".to_owned()
            )],
            "the app is asked, and told who is asking"
        );
        assert!(call(&snapshot, &probe, "sessions_list", None, "an agent").is_ok());
        assert_eq!(probe.asked().len(), 1, "a read tool never reaches the app");
    }

    #[test]
    fn a_refused_action_is_reported_as_the_apps_own_refusal() {
        let error = call(
            &sample(),
            &Probe::refusing(),
            "session_start",
            Some(&json!({})),
            "an agent",
        )
        .expect_err("refused");
        assert_eq!(error.kind, ErrorKind::Denied);
        assert_eq!(error.param("action"), Some("session_start"));
        assert_eq!(error.param("reason"), Some("refused"));
    }

    #[test]
    fn an_empty_list_says_so_when_the_app_has_not_looked_yet() {
        let snapshot = Snapshot {
            notes: vec!["the runtimes have not been listed yet".to_owned()],
            ..Snapshot::default()
        };
        let answer = call(
            &snapshot,
            &Probe::default(),
            "sessions_list",
            None,
            "a client",
        )
        .expect("answers");
        assert!(answer["sessions"].as_array().expect("a list").is_empty());
        assert_eq!(
            answer["notes"][0], "the runtimes have not been listed yet",
            "an agent must not read nothing-is-running out of not-looked"
        );
        // Nothing to say, nothing added: a tool result stays the shape it promises.
        let quiet = call(
            &Snapshot::default(),
            &Probe::default(),
            "sessions_list",
            None,
            "c",
        )
        .expect("answers");
        assert!(quiet.get("notes").is_none());
    }

    #[test]
    fn the_control_plane_answers_without_credentials() {
        let snapshot = Snapshot {
            accounts: vec![
                Account {
                    email: "a@b.c".to_owned(),
                    active: true,
                },
                Account {
                    email: "c@d.e".to_owned(),
                    active: false,
                },
            ],
            jobs: vec![Job {
                plugin: "batch".to_owned(),
                session: "run-1".to_owned(),
                state: "running".to_owned(),
                stage: "train".to_owned(),
            }],
            downstream: vec![
                Downstream {
                    plugin: "llama-cpp".to_owned(),
                    endpoint: "api".to_owned(),
                    kind: "openai".to_owned(),
                    base_url: "http://127.0.0.1:8000/v1".to_owned(),
                    model: "org/Qwen3-0.6B".to_owned(),
                    healthy: true,
                },
                Downstream {
                    plugin: "llama-cpp".to_owned(),
                    endpoint: "leaky".to_owned(),
                    kind: "openai".to_owned(),
                    base_url: "http://127.0.0.1:8000/v1?colab-runtime-proxy-token=secret"
                        .to_owned(),
                    model: "refresh_token=abc".to_owned(),
                    healthy: true,
                },
                Downstream {
                    plugin: "other".to_owned(),
                    endpoint: "api".to_owned(),
                    kind: "http".to_owned(),
                    base_url: "http://example.invalid:8000/v1".to_owned(),
                    model: "m".to_owned(),
                    healthy: false,
                },
            ],
            ..Snapshot::default()
        };
        let probe = Probe::default();
        let accounts =
            call(&snapshot, &probe, "accounts_list", None, "a client").expect("accounts");
        assert_eq!(accounts["accounts"][0]["email"], "a@b.c");
        assert_eq!(accounts["accounts"][0]["active"], true);
        assert!(accounts["accounts"][0].get("token").is_none());
        let job = call(
            &snapshot,
            &probe,
            "job_status",
            Some(&json!({ "plugin": "batch" })),
            "a client",
        )
        .expect("job");
        assert_eq!(job["state"], "running");
        assert_eq!(job["stage"], "train");
        let missing = call(
            &snapshot,
            &probe,
            "job_status",
            Some(&json!({ "plugin": "llama-cpp" })),
            "a client",
        )
        .expect_err("a service is not a job");
        assert_eq!(missing.kind, ErrorKind::InvalidInput);
        let config =
            call(&snapshot, &probe, "downstream_config", None, "a client").expect("config");
        let tools = config["tools"].as_array().expect("tools");
        assert_eq!(
            tools.len(),
            1,
            "a public host and a tokenised URL are dropped: {config}"
        );
        assert_eq!(tools[0]["base_url"], "http://127.0.0.1:8000/v1");
        assert_eq!(tools[0]["model"], "org/Qwen3-0.6B");
        assert_eq!(tools[0]["healthy"], true);
        assert!(!config.to_string().contains("proxy"), "{config}");
        assert!(!config.to_string().contains("token"), "{config}");
        let blanked = Snapshot {
            downstream: vec![Downstream {
                plugin: "llama-cpp".to_owned(),
                endpoint: "api".to_owned(),
                kind: "openai".to_owned(),
                base_url: "http://127.0.0.1:8000/v1".to_owned(),
                model: "refresh_token=abc".to_owned(),
                healthy: false,
            }],
            ..Snapshot::default()
        };
        let hidden = call(&blanked, &probe, "downstream_config", None, "a client").expect("kept");
        assert_eq!(hidden["tools"][0]["model"], "");
        assert_eq!(hidden["tools"][0]["base_url"], "http://127.0.0.1:8000/v1");
        assert!(probe.asked().is_empty(), "these reads never reach the app");
    }

    #[test]
    fn a_tool_never_answers_with_anything_but_an_object() {
        let snapshot = sample();
        let probe = Probe::default();
        for tool in all() {
            let arguments = arguments(tool.name);
            let answer = call(&snapshot, &probe, tool.name, arguments.as_ref(), "a client")
                .unwrap_or_else(|error| panic!("{}: {error}", tool.name));
            assert!(answer.is_object(), "{}", tool.name);
        }
    }
}
