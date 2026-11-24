//! The tools an agent may call, and the arguments each one accepts.
//!
//! Slice 1 is read-only: every tool here answers from a [`Snapshot`](super::view::Snapshot) and
//! none of them changes anything, spends anything or starts a session, so the endpoint can be
//! switched on for real from the day it lands. A tool that would do any of those is slice 2, and
//! the confirmation it needs is the app's own dialog.
//!
//! Descriptions are written for a model that has never seen this app: what the tool answers, and
//! what it does *not* do, because a caller that expects a side effect here would be wrong.

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};

use super::view::Snapshot;

/// One tool: its name, what it is for, the arguments it takes and how it answers.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// The JSON Schema for the arguments. A function so the table stays free of `Value`s.
    pub input: fn() -> Value,
    /// Answers from the snapshot, or fails with an error the caller reports as one.
    pub read: fn(&Snapshot, &Args) -> Result<Value>,
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

fn plugin_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": {
                "type": "string",
                "description": "The plugin id, as plugin_list answers it."
            }
        },
        "required": ["plugin"],
        "additionalProperties": false
    })
}

/// The tools of this slice, in the order `tools/list` reports them.
pub fn all() -> &'static [Tool] {
    &[
        Tool {
            name: "sessions_list",
            description: "The Colab runtimes of every signed-in account: name, hardware, state, \
                          when each stops by itself and what it costs per hour. Read-only: nothing \
                          is started or stopped.",
            input: empty_schema,
            read: sessions_list,
        },
        Tool {
            name: "session_usage",
            description: "Each account's compute-unit balance and the rate it is burning at right \
                          now. Read-only.",
            input: empty_schema,
            read: session_usage,
        },
        Tool {
            name: "runs_list",
            description: "The plugin runs the app knows about: which plugin, on which session, and \
                          how far each has got. Read-only.",
            input: empty_schema,
            read: runs_list,
        },
        Tool {
            name: "run_status",
            description: "One plugin run: its stage, whether it has said it is ready, and the tail \
                          of its log (secrets already redacted). Read-only.",
            input: plugin_schema,
            read: run_status,
        },
        Tool {
            name: "run_outputs",
            description: "What one plugin run published: the workflow's nodes and links, and the \
                          paths of the files it produced on the runtime. Read-only; downloading \
                          those files is not part of this tool.",
            input: plugin_schema,
            read: run_outputs,
        },
        Tool {
            name: "plugin_list",
            description: "The plugins installed in this app: id, name, whether each is enabled, and \
                          whether it ships with the app. Read-only.",
            input: empty_schema,
            read: plugin_list,
        },
        Tool {
            name: "settings_get",
            description: "The app's settings: language, appearance, the hardware and auto-stop new \
                          sessions ask for, and host details. Read-only.",
            input: empty_schema,
            read: settings_get,
        },
        Tool {
            name: "accounts_list",
            description: "The Google accounts signed in on this machine and which one is active. \
                          Read-only; switching accounts is not part of this tool.",
            input: empty_schema,
            read: accounts_list,
        },
        Tool {
            name: "forwards_list",
            description: "The port forwards that are open through sessions, with the local port an \
                          agent would connect to. Read-only.",
            input: empty_schema,
            read: forwards_list,
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
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": (tool.input)(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Runs one tool. An unknown name is an error the caller reports as a JSON-RPC fault; a tool that
/// refuses its arguments reports that as a tool result, since the client did reach the tool.
pub fn call(snapshot: &Snapshot, name: &str, arguments: Option<&Value>) -> Result<Value> {
    let tool =
        find(name).ok_or_else(|| Error::invalid("name", format!("no tool called {name}")))?;
    let args = Args::parse(arguments)?;
    (tool.read)(snapshot, &args)
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
    Ok(json!({ "accounts": snapshot.accounts }))
}

fn forwards_list(snapshot: &Snapshot, args: &Args) -> Result<Value> {
    args.only(&[])?;
    Ok(json!({ "forwards": snapshot.forwards }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::view::{Output, Plugin, Run, Session, Workflow};

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
            ..Snapshot::default()
        }
    }

    #[test]
    fn every_tool_is_named_uniquely_and_explains_itself() {
        let mut names: Vec<&str> = all().iter().map(|tool| tool.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two tools share a name");
        assert_eq!(count, 9, "the tools of slice 1");
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
        }
    }

    #[test]
    fn tools_list_reports_what_the_table_holds() {
        let listed = descriptors();
        assert_eq!(listed["tools"].as_array().expect("tools").len(), 9);
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert!(names.contains(&"sessions_list"));
        assert!(names.contains(&"run_outputs"));
    }

    #[test]
    fn a_read_tool_answers_from_the_snapshot() {
        let snapshot = sample();
        let listed = call(&snapshot, "sessions_list", None).expect("answers");
        assert_eq!(listed["sessions"][0]["hardware"], "T4");
        let status = call(
            &snapshot,
            "run_status",
            Some(&json!({ "plugin": "comfyui" })),
        )
        .expect("answers");
        assert_eq!(status["stage"], "ready");
        assert_eq!(status["ready"], true);
        let outputs = call(
            &snapshot,
            "run_outputs",
            Some(&json!({ "plugin": "comfyui" })),
        )
        .expect("answers");
        assert_eq!(
            outputs["outputs"][0]["path"],
            "/content/ComfyUI/output/image.png"
        );
    }

    #[test]
    fn an_empty_state_answers_with_empty_lists_not_an_error() {
        for (name, key) in [
            ("sessions_list", "sessions"),
            ("session_usage", "usage"),
            ("runs_list", "runs"),
            ("plugin_list", "plugins"),
            ("accounts_list", "accounts"),
            ("forwards_list", "forwards"),
        ] {
            let answer = call(&Snapshot::default(), name, None).expect(name);
            assert!(answer[key].as_array().expect(name).is_empty(), "{name}");
        }
    }

    #[test]
    fn an_unknown_tool_names_itself_in_the_error() {
        let error = call(&Snapshot::default(), "session_start", None).expect_err("unknown");
        assert_eq!(error.kind, crate::error::ErrorKind::InvalidInput);
        assert!(error.detail.contains("session_start"), "{}", error.detail);
    }

    #[test]
    fn a_run_that_does_not_exist_is_an_error_not_an_empty_answer() {
        let error = call(
            &Snapshot::default(),
            "run_status",
            Some(&json!({ "plugin": "comfyui" })),
        )
        .expect_err("no such run");
        assert!(error.detail.contains("runs_list"), "{}", error.detail);
    }

    #[test]
    fn arguments_are_checked_strictly() {
        let snapshot = sample();
        // A missing required argument.
        assert!(call(&snapshot, "run_status", Some(&json!({}))).is_err());
        // An argument of the wrong type.
        assert!(call(&snapshot, "run_status", Some(&json!({ "plugin": 7 }))).is_err());
        // An argument this tool does not take.
        assert!(
            call(&snapshot, "sessions_list", Some(&json!({ "all": true }))).is_err(),
            "a caller that misreads the tool is told"
        );
        // Arguments that are not an object at all.
        assert!(call(&snapshot, "sessions_list", Some(&json!([1, 2]))).is_err());
    }

    #[test]
    fn a_tool_never_answers_with_anything_but_an_object() {
        let snapshot = sample();
        for tool in all() {
            let arguments = match tool.name {
                "run_status" | "run_outputs" => Some(json!({ "plugin": "comfyui" })),
                _ => None,
            };
            let answer = call(&snapshot, tool.name, arguments.as_ref()).expect(tool.name);
            assert!(answer.is_object(), "{}", tool.name);
        }
    }
}
