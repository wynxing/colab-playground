# The agent waits, and the app tells it when something happens

Status: draft, agreed in outline with the user (their words: *"希望能设计成 agent 能接收消息"*).

## What is wrong today

An agent that drives the app can only ask questions. Every answer it gets is the state of that
moment, so a job that takes four minutes is either polled (one call per stage, each answered with
"nothing new yet") or waited on inside a single call, which the app holds open until the work lands.

The second shape breaks on a client that gives up. The MCP client on this machine abandons a call
after 300 seconds; the app cannot change that (asked for and not found in OpenBot's settings, and
reported upstream). A runtime that took over five minutes to come up answered nobody, and the agent
could not tell a slow job from a failed one — it may start a second, paid runtime.

## The three shapes, and what each can carry

**Wait (chosen).** The agent calls a tool that answers the moment something it asked about changes,
and answers again on the next call. This is the only shape that works over the transport OpenBot
holds: a streamable-HTTP MCP client reads server messages only while a request is open, so a message
that no call is waiting for has nowhere to arrive.

**Push.** The app sends a notification when the agent has a call open. Useful, and cheap once the
wait exists — the same event that answers a wait is also a notification — but it cannot reach an
agent that is not waiting. So it is an addition, never the base.

**The user's own words.** The user types something in the app that the agent should see (for
example "when the render is done, download it for me"). That is a different feature with its own
questions — where the words are stored, who may write them, how the agent is told there are any —
and is deliberately left out of this piece.

## The design: `run_watch`

One tool, for the thing that takes time:

```
run_watch { plugin, since? }  ->  { run: { … }, changed: "stage" | "ready" | "failed" | "gone" | "none" }
```

* It answers as soon as the run's stage or state is not what it was when the call arrived: a new
  stage, ready, failed, an output written, or the runtime going away.
* It answers after roughly two minutes even when nothing changed, with `changed: "none"` and the
  run as it stands. That is the point of it: **the app always answers well inside a client's
  timeout**, so a long job is a series of short, certain calls rather than one call that may be
  abandoned.
* It is a read: it changes nothing, says so in its description, and needs no approval.
* Calling it again is the way to keep watching. No cursor is needed: the answer is the run as it is,
  and the agent compares it with the last one it had.

The shape of the work it makes possible — the whole of a long job without polling:

```
plugin_run { plugin: "comfyui", … }        -> answers at the first stage (already shipped)
run_watch  { plugin: "comfyui" }           -> "stage: install"
run_watch  { plugin: "comfyui" }           -> "changed: ready"
run_download { plugin: "comfyui" }         -> the files, on this computer
```

### Where it lives

* `crates/core/src/mcp/actions.rs`: `Action::RunWatch { plugin }`, `name()` `run_watch`, `target()`
  the plugin, `is_paid()` false.
* `crates/core/src/mcp/tools.rs`: the row, `readOnlyHint: true`, and a description that says in
  words that it answers on a change or after about two minutes, whichever comes first.
* `crates/app/src/state/agents.rs`: the wait is a pending call the window keeps — not an `Op`,
  because no work is being done — answered by the app's own events:
  * the run's next status or phase change (`apply_run_event`, `run_became_ready`, `finish_run`);
  * the runtime going away (`stop_runtime`, the guard, a lost session);
  * a deadline of its own (about two minutes), set like the approval's, so the answer never depends
    on something happening.
* Only one wait per client at a time, and a second one is answered `busy`, the same rule the paid
  calls already follow.
* The answer's `changed` field is what tells the agent whether to look again at once or to keep
  waiting; `run` is the same view `run_status` gives, so there is one shape to learn.

### What it does not do

* It does not watch a session's guard or a forward's health; those are the same shape and can follow
  when there is a reason.
* It does not keep a history, so an agent that drops a call loses nothing but that call.
* It does not push: notifications, when added, ride on the same events.

## How it will be known to work

* A window test that drives the endpoint over a real socket: a run is started, `run_watch` is called,
  and the call is answered when the run reaches its next stage — and again, with `changed: "none"`,
  when the deadline passes with nothing happening.
* A test that a second wait from the same client while one is pending is answered `busy`.
* A test that a wait on a plugin with no run is answered `plugin_not_found` rather than waiting.
* The live check the earlier pieces got: start a runtime, deploy ComfyUI, watch it to ready, download
  the image.
