# A local MCP endpoint (slice 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** an agent connected to `127.0.0.1` can drive the app: it starts and stops runtimes, extends an auto-stop, deploys a plugin, reads a run's files down to this computer, opens and closes forwards, changes the settings, switches account, enables or installs a plugin — and every action that spends money or installs code asks in the app's own window first, and waits for the answer.

**Architecture:** `core` owns the *vocabulary*: an `Action` is what an agent asked for once its arguments were checked, and the `Actions` trait is the one thing the endpoint calls to have it done. `core` still changes nothing and asks nobody. The app implements `Actions` as a **bridge**: the call arrives on the endpoint's own thread, travels to the foreground, and is answered there — through the same `AppState::run_backend(op, …)` path a button press takes, so the busy state, the one-call-per-`Op` rule and the result all belong to the window rather than to a second implementation beside it. An action that needs the user's word is the app's own confirmation dialog, held open while the agent's HTTP response waits.

**Tech Stack:** Rust (workspace `edition 2024`, `rust-version 1.98`), `std::sync::mpsc` and `std::thread`, the workspace's `serde`/`serde_json`, GPUI and Ely components for the dialog.

**Spec:** `docs/superpowers/specs/2026-11-24-agent-mcp-endpoint-design.md` (Approval, Errors and concurrency, The tools — slice 2)

**Predecessor:** `docs/superpowers/plans/2026-11-24-agent-mcp-endpoint-slice-1.md` (landed)

## Status

**Landed.** Tasks 1–3 are on `main` (slice 1's pull request): the action vocabulary, the eleven write
tools with their argument checks, and the endpoint's half of a call — a session remembers which
client opened it, and an action travels to the app with that client's name on it. `ErrorKind::Denied`
(task 4) landed with them, because a refusal is one of the things the endpoint has to report and the
vocabulary was not finished without it.

Tasks 5–10 are the app's half, on `feat/agent-mcp-driving` (a pull request apart from slice 1):
`state/agents.rs` holds the bridge (a queue the endpoint puts calls on and waits on), the dispatch
that turns an action into the `Op` a click starts, the answers built from the state the window
shows, and `Overlay::Approval` — the dialog a call that spends money waits on. Where a checkbox
below disagrees with the code, the code is what happened.

Four things this plan did not say, and the code does:

- **A settings change has no `Op`.** `settings_set` is a file write with nothing to wait for, so it
  is answered in the same turn rather than filed against an `Op` that never starts.
- **A call is filed after the work starts.** A call filed before a `perform` that then decided not
  to start would wait for an `Op` that is never coming; the guard right after answers the agent
  instead of leaving it to the backstop.
- **`plugin_install` is not gated by the approval dialog.** The design gates what spends money, and
  an install spends none: what an agent gets is the app's own manifest check first, whose complaint
  is the answer, so nothing is installed that the window's own dialog would have refused.
- **The dialog's tests are the window's** (`crates/app/tests/ui/agents.rs`), not the state module's
  as task 7 said: a dialog is a frame, and only the harness that draws one can click its buttons.
  `state/agents.rs` keeps the unit tests that need no window — the bridge refusing, the answers'
  parsers, the forward's key.
- **`session_extend` carries the minutes it was asked for.** The Extend button adds the app's own
  duration, which is why one shared `extend_runtime` takes the duration as an argument; the agent
  gets the number it named and the `Op` is the same either way.

## Global Constraints

Every rule of the slice-1 plan holds here unchanged: the `1.99.0` toolchain (the workspace wants
`1.98`), no new dependencies, `crate::error::{Error, ErrorKind, Result}` for every failure, every
string in the window through `keys::` and all five catalogs, every interface element carrying a
`tagged` selector and a test, no bare number in `crates/app/src/ui`, no module but `platform.rs`,
`host.rs` and `paths.rs` naming a target OS, `fmt --all --check` + `clippy --workspace --all-targets
--locked -- -D warnings` + `cargo test --workspace --locked` before every commit, conventional-commit
subjects with no agent attribution, nothing pushed to `main`.

Three rules are new, and they are what this slice is made of:

- **A tool call takes the path a click takes.** The app's half of a tool call calls the same
  `AppState` method the button calls, so it starts the same `Op` and lands in the same place. It
  never calls `Backend` directly: an agent's runtime that the window does not know about would be
  a second truth, which is the whole reason slice 1 reads a published snapshot.
- **The window asks before anything spends money.** Starting, stopping, extending or running
  something asks in the app's own dialog, naming the action, its target, its hourly rate where
  there is one and the client that asked. Refused, dismissed or timed out is an error marked as a
  refusal, and that answer is final — the agent must not call again.
- **A refusal is a kind of its own.** `ErrorKind::Denied` (params `action`, `reason`) with
  `NextStep::None`: no existing kind means "the person said no, and asking again is wrong".

---

### Task 1: What an agent may ask for — LANDED

**Files:**
- Create: `crates/core/src/mcp/actions.rs`
- Modify: `crates/core/src/mcp/mod.rs`

**Interfaces:**
- Produces: `Action` (a plain enum: `SessionStart { name, hardware, auto_stop_minutes }`,
  `SessionStop { session }`, `SessionExtend { session, minutes }`,
  `PluginRun { plugin, session, inputs }`, `RunDownload { plugin }`, `SettingsSet { changes }`,
  `AccountsSwitch { account }`, `PluginEnable { plugin, enabled }`, `PluginInstall { source }`,
  `ForwardOpen { session, remote_port, local_port }`, `ForwardClose { session, local_port }`),
  `Action::name() -> &'static str` (the tool it came from), `Action::is_paid() -> bool`,
  `trait Actions: Send + Sync { fn perform(&self, action: Action, client: &str) -> Result<Value> }`,
  `struct Unavailable` (an `Actions` that refuses everything, for tests and for an app that is not
  there).

- [x] **Step 1: Failing test — an action names the tool it came from, and only the four that spend
  money are paid** (`crates/core/src/mcp/actions.rs`, `mod tests`).
- [x] **Step 2: Failing test — `Unavailable` refuses with a machine-readable error, and never
  panics**, whatever it is handed.
- [x] **Step 3: Implement** the enum, `name`, `is_paid`, the trait and `Unavailable`.
- [x] **Step 4: Run** `rustup run 1.99.0 cargo test -p colab-playground-core --lib mcp::actions`.
- [x] **Step 5: Commit** `feat(core): let an agent ask the app to do something` (with tasks 1–3).

### Task 2: The write tools and their argument checks — LANDED

**Files:**
- Modify: `crates/core/src/mcp/tools.rs`

**Interfaces:**
- Consumes: `actions::Action`.
- Produces: `Tool { name, description, input, kind }` where
  `enum Kind { Read(fn(&Snapshot, &Args) -> Result<Value>), Write(fn(&Args) -> Result<Action>) }`,
  `Tool::is_paid() -> bool` (true for the four whose action is paid), `call` gaining
  `call(&Snapshot, &dyn Actions, &str, Option<&Value>, client: &str) -> Result<Value>`, and
  `Args` gaining what the new arguments need: `flag`, `number`, `limit`, `port`, `texts` (the
  manifest inputs of a plugin run, kept as the JSON they arrived as).

- [x] **Step 1: Failing test — the table holds every tool of this slice, each with a schema that
  refuses surplus arguments** (`every_tool_is_named_uniquely_and_explains_itself` grows the count).
- [x] **Step 2: Failing test — every write tool, called with sample arguments, produces the action
  it says it does, and every tool marked paid produces a paid action**
  (`a_write_tool_checks_its_arguments_into_an_action`).
- [x] **Step 3: Failing test — a write tool refuses bad arguments before any action exists**
  (`a_write_tool_refuses_bad_arguments_before_anything_happens`).
- [x] **Step 4: Failing test — a write tool goes to the executor and answers with what it returns,
  and a read tool never reaches it** (`a_write_tool_goes_to_the_executor`).
- [x] **Step 5: Implement** the write tools: `run_download`, `settings_set`, `accounts_switch`,
  `plugin_enable`, `plugin_install`, `forward_open`, `forward_close`, `session_start`,
  `session_stop`, `session_extend`, `plugin_run`. Descriptions say what the tool does, that a
  paid one asks the user in the app's window first, and what a refusal means.
- [x] **Step 6: Run** `--lib mcp::tools`.
- [x] **Step 7: Commit** with task 1's commit.

### Task 3: The endpoint hands an action to the app — LANDED

**Files:**
- Modify: `crates/core/src/mcp/server.rs`, `crates/core/tests/mcp_endpoint.rs`

**Interfaces:**
- Consumes: `actions::{Action, Actions}`.
- Produces: `Config { token, port, actions: Arc<dyn Actions> }`, `Ctx::actions`, and a session's
  `Client { name, version }` read from `initialize`'s `clientInfo` and kept beside the session id.
  The wait belongs to the app's bridge (task 5), not here: only the window can tell "still working"
  from "gone".

- [x] **Step 1: Failing test (in-file) — a session is remembered with the client that opened it,
  and the client that asked is the one the action is performed for** (`the_client_that_asked_is
  _named_with_the_action`).
- [x] **Step 2: Failing test (in-file) — a write tool's refusal is a tool result marked as an
  error, not a fault** (`a_refused_action_is_a_result_marked_as_an_error`).
- [x] **Step 3: Failing test (integration, real socket) — a write tool over one connection reaches
  the executor and its answer comes back** (`an_agent_drives_the_app_over_a_socket`).
- [x] **Step 4: Implement** the wiring: `initialize` records the client, `tools/call` sends a write
  tool's action to `Ctx::actions` with the client's name, and `Endpoint::start` takes the executor.
- [x] **Step 5: Run** `--test mcp_endpoint` and `--lib mcp`.
- [x] **Step 6: Commit** with task 1's commit.

### Task 4: `ErrorKind::Denied`

**Files:**
- Modify: `crates/core/src/error.rs`

**Interfaces:**
- Produces: `ErrorKind::Denied`, whose `default_next_step` is `NextStep::None`, and
  `Error::denied(action: &str, reason: Denial) -> Error` with `Denial::{Refused, Dismissed,
  TimedOut, Busy}` as its `reason` param.

- [x] **Step 1: Failing test** in `error.rs`'s tests: a denial carries the action and the reason,
  its kind is `denied`, and its next step is none — there is nothing for the agent to try.
- [x] **Step 2: Implement** the kind, the helper and the `Denial` reason.
- [x] **Step 3: Run** `cargo test -p colab-playground-core --lib error`.
- [x] **Step 4: Commit** `feat(core): refuse an agent's call in the app's own words`.

### Task 5: The app's bridge — what an agent's call travels on

**Files:**
- Create: `crates/app/src/state/agents.rs` (the bridge and the calls)
- Modify: `crates/app/src/state/mcp.rs` (the endpoint is started with it), `crates/app/src/state/mod.rs`

**Interfaces:**
- Consumes: `colab_playground_core::mcp::actions::{Action, Actions}`.
- Produces: `Bridge` (implements `Actions`: sends a `Call` to the foreground and blocks on the
  answer), `struct Call { action, client, reply }`, `AppState::agent_calls` (the drain), and
  `AppState::wait_for_agent_call(op, call, cx)` / `AppState::settle_agent_call(op, cx)`.

- [x] **Step 1: Failing test** in `agents.rs`'s tests over the mock backend: a call made while no
  window is draining it is refused rather than hanging (`the_bridge_refuses_when_the_app_is_not
  _listening`), and a call that is answered comes back with its answer
  (`a_call_comes_back_with_the_foreground_answer`).
- [x] **Step 2: Implement `Bridge`** — a `Sender<Call>` and a generous ceiling (`CALL_WAIT`,
  documented as a backstop: the app's own timeouts are the real ones, and a dropped receiver
  answers at once).
- [x] **Step 3: Implement the drain** — a foreground task started with the endpoint and dropped
  with it, which takes each waiting call and begins it on the foreground. A call that outlives its
  own deadline is answered as a timeout.
- [x] **Step 4: Wire it into the endpoint's life** (`mcp::Agents::sync` starts the endpoint with the
  bridge), and stop the drain when the endpoint stops.
- [x] **Step 5: Run** `cargo test -p colab-playground --locked`.
- [x] **Step 6: Commit** `feat: let the endpoint ask the window to do the work`.

### Task 6: One call per `Op`, and the answer when it lands

**Files:**
- Modify: `crates/app/src/state/agents.rs`, `crates/app/src/state/ops.rs` (one hook), the state
  modules whose methods the actions call

**Interfaces:**
- Consumes: `Op`, `key_of`.
- Produces: `AppState::begin_agent_call(call, cx)` (paid ones open the dialog, the rest run),
  `AppState::perform_agent_call` (the action → the state method the button calls, and the `Op` it
  starts), `AppState::settle_agent_call(op, cx)` called from `run_backend`'s and
  `run_backend_streaming`'s completion, `answers::for_action(action, state) -> Result<Value>`.

- [x] **Step 1: Failing test** — an action whose `Op` is already running is answered `Busy` at once
  and starts nothing (`a_second_call_about_the_same_thing_is_busy_not_queued`).
- [x] **Step 2: Failing test** — an action that runs leaves the same traces a click leaves: the op
  is running while it runs (`is_busy`), and the state it changed is the state the window shows
  (`a_call_takes_the_same_path_a_click_takes`).
- [x] **Step 3: Implement the dispatch** for the non-paid tools, each naming the `Op` it starts:
  `run_download` → `Op::DownloadOutputs(key_of(path))` via `save_run_outputs`,
  `settings_set` → the settings save (answered at once, no `Op`),
  `accounts_switch` → `Op::Switch` via `request_switch` + `confirm_switch`,
  `plugin_enable` → `Op::PluginChange(key_of(id))` via `set_plugin_enabled`,
  `forward_open` → `Op::Forward(key_of(spec))` via `open_tunnel`,
  `forward_close` → `Op::CloseForward(key_of(id))` via `close_forward`.
- [x] **Step 4: Implement `answers::for_action`** — the answer to each action is built from the
  state the action changed: the local paths a download wrote, the settings after a save, the
  plugin list, the forward, the runtime. The rule is the snapshot's: what the window shows.
- [x] **Step 5: Implement the settlement hook** in `ops.rs` after `done(...)`, so a call's answer is
  the state as it stands once the call it waited for has landed.
- [x] **Step 6: Run** `cargo test --workspace --locked`.
- [x] **Step 7: Commit** `feat: let an agent do what a click does`.

### Task 7: The paid tools, and the dialog that gates them

**Files:**
- Create: `crates/app/src/ui/overlays/approval.rs`
- Modify: `crates/app/src/state/agents.rs`, `crates/app/src/state/mod.rs` (`Overlay::Approval`),
  `crates/app/src/ui/overlays/mod.rs`, `crates/app/src/i18n/mod.rs` and all five catalogs

**Interfaces:**
- Produces: `ApprovalPrompt { client, action, target, units_per_hour, error }`, `Overlay::Approval`,
  `AppState::allow_agent_call`, `AppState::refuse_agent_call(reason)`, `AppState::agent_approval()`,
  `APPROVAL_ALLOW` / `APPROVAL_REFUSE` selectors, `APPROVAL_WAIT` (two minutes),
  and the keys `APPROVAL_TITLE`, `APPROVAL_BODY`, `APPROVAL_CLIENT`, `APPROVAL_RATE`,
  `APPROVAL_ALLOW`, `APPROVAL_REFUSE`, `APPROVAL_REFUSED`, `APPROVAL_CLOSED`.

- [x] **Step 1: Failing test** in `agents.rs`: a paid call opens the dialog and starts nothing; the
  rate the hardware would burn is on it (`a_paid_call_asks_first_and_names_what_it_costs`).
- [x] **Step 2: Failing test**: a second paid call while one waits is refused at once
  (`a_second_paid_call_is_refused_while_one_waits`).
- [x] **Step 3: Failing test**: refusing answers the agent with a denial, dismisses the dialog and
  leaves the state untouched; letting the deadline pass does the same with `TimedOut`
  (`refusing_and_timing_out_are_final`).
- [x] **Step 4: Build the dialog** the way `stop.rs` builds its own, with the client's own name and
  the hourly rate where there is one, naming the action and its target.
- [x] **Step 5: Implement the four paid actions** — `session_start` → `Op::Provision`,
  `session_stop` → `Op::Stop(key_of(target))`, `session_extend` → `Op::Extend(key_of(target))`,
  `plugin_run` → `Op::PluginRun`.
- [x] **Step 6: Add the keys** in all five catalogs.
- [x] **Step 7: Run** `cargo test --workspace --locked` (including the i18n tests).
- [x] **Step 8: Commit** `feat(app): ask before an agent spends money`.

### Task 8: `plugin_install`, which asks once and then checks before it installs

**Files:**
- Modify: `crates/app/src/state/agents.rs`, `crates/app/src/state/plugins.rs`

**Interfaces:**
- Consumes: `inspect_plugin`, `confirm_install`.
- Produces: `AppState::install_for_agent` (the two-stage call: the user's approval covers the
  install, so the inspect prompt is not shown a second time — the manifest's own confirmation
  dialog belongs to that install once and this is it).

- [x] **Step 1: Failing test** — an approved `plugin_install` runs `Op::InspectPlugin`, then the
  install, and answers with the plugin list; a manifest the app refuses is answered with that
  error and installs nothing (`an_approved_install_checks_before_it_installs`).
- [x] **Step 2: Implement** the two stages, with the second only entered when the first found a
  plugin to install.
- [x] **Step 3: Run** `cargo test -p colab-playground --locked`.
- [x] **Step 4: Commit** `feat(app): install a plugin for an agent after one question`.

### Task 9: The window's own tests of a driving agent

**Files:**
- Modify: `crates/app/tests/ui/agents.rs`

**Interfaces:**
- Consumes: `support::TestApp`, the endpoint's address and token, and a plain `TcpStream`.

- [x] **Step 1: Test — a read tool over a real socket answers while the window is fine**, and the
  tool call shows as the busy state a click makes (`an_agent_reads_over_a_socket`).
- [x] **Step 2: Test — a paid call parks the window with the dialog up, and letting it through runs
  it** (`an_agent_is_answered_when_the_user_allows_it`).
- [x] **Step 3: Test — refusing ends it**: the agent's answer is a denial, the dialog is gone and
  nothing was started (`a_refused_call_starts_nothing`).
- [x] **Step 4: Test — the agent's own name is on the dialog**, and a second paid call is refused
  while one waits.
- [x] **Step 5: Run** `cargo test -p colab-playground --locked`.
- [x] **Step 6: Commit** `test(app): cover an agent driving the window`.

### Task 10: What the window and the docs say about a driving agent

**Files:**
- Modify: `crates/app/src/ui/pages/settings.rs` (the section's description), `docs/USAGE.md`,
  `docs/ROADMAP.md`

- [x] **Step 1: The Settings section** says what an agent can *do* now, not only what it can read:
  the sentence that said "read" gains the actions, and the paid ones are named as asking here.
- [x] **Step 2: USAGE** — a short "Let an agent drive it": what it can do, that the app asks before
  anything is spent, and what a refusal means.
- [x] **Step 3: ROADMAP** — slice 3 (`run_watch` over the `GET` stream, reattaching after a
  restart) is what is left.
- [x] **Step 4: Run** `cargo test --workspace --locked`.
- [x] **Step 5: Commit** `docs: what an agent can drive`.

---

## Self-review

- **Spec coverage.** The tools of the spec's slice 2 are all here: `run_download`, `settings_set`,
  `accounts_switch`, `plugin_enable`, `plugin_install`, `forward_open`, `forward_close`,
  `session_start`, `session_stop`, `session_extend`, `plugin_run`. `run_watch` is slice 3 and is
  not in this plan. Approval, the two-minute wait, the finality of a refusal, one call per `Op`,
  and the busy answer are tasks 4, 6 and 7.
- **Two decisions this plan makes that the spec did not.** (1) A write tool answers when the work
  it started has landed, not when it was accepted: an agent told "accepted" would have to poll, and
  `run_watch` (slice 3) is the thing that polls. (2) The endpoint waits for the app for a long
  ceiling and the app owns both timeouts — the two minutes of approval and the run itself — because
  only the window can tell "still working" from "gone".
- **What is deliberately not tested.** A real third-party client driving a paid tool: the socket
  tests here are the app's own, and the one real client handshake was slice 1's. Lighting up a real
  Colab runtime from a test would bill the owner, which `DESIGN.md`'s hard safety rules forbid.
- **Placeholders:** none; every task names its files, its test and the command that proves it.
- **Landing:** two pull requests, because they can be reviewed apart: tasks 1–3 (core: the
  vocabulary and the endpoint's half) and tasks 4–10 (the app: the bridge, the dialog, the tools'
  own work and the window's tests). The spec's slice number stays one thing.
